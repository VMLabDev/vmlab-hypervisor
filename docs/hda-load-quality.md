# HDA playback under load

The isolated 30-second Windows CPU/D3D11 load acceptance passes through both
the paced backend and a private PipeWire null sink. Three consecutive traced
PipeWire repeats also pass. Each full tone has zero discontinuities, zero gaps,
30 correctly spaced clicks, and -75.85 dB THD+N.

## Reproduction

The opt-in S1 tests boot disposable Windows 11 win3d overlays. A native WASAPI
client generates a 997 Hz left-channel sine and right-channel clicks once per
second. Exclusive event-driven playback requests 480-frame periods at 48 kHz.
MMCSS schedules the audio thread at critical Pro Audio priority on guest CPU 3.
Three below-normal-priority native workers, pinned to guest CPUs 0 through 2,
keep all but one of the four guest CPUs busy. A separate D3D11 hardware process
repeatedly clears a 1920 by 1080 render target and waits for GPU completion.
The test requires an explicitly selected render node and a win3d template.

One second of silent preroll lets the driver fill its cyclic buffer before the
full 30-second signal starts. A 250 ms silent tail follows. The analyser does
not trim any tone samples. This test does not qualify cold-start playback.
The native client records every fill's FILETIME, source offset, frame count,
buffer pointer, and HRESULT in memory. It writes CSV only after stopping audio,
so diagnostic disk I/O cannot delay the real-time source. GPU readiness and
completed batch counts are checked. No PCI or USB device is claimed.

The file-backed test tap forwards PCM and queue measurements to the real
backend when `VMLAB_HV_TEST_AUDIO_TEE=1`. A private PipeWire daemon and a
policy-only WirePlumber instance connect playback to a null sink. `pw-record`
records that sink's monitor as raw S16LE stereo. Neither process discovers host
hardware or connects to the workstation's sound server.

The alternate load test uses the existing paced recording backend. Both tests
assert the submitted PCM and rendered output. The analyser counts second
differences above 1.5 times the nominal sine's maximum curvature plus four
quantization units. It reports the eight most common distances between spikes.
Adjacent pairs can identify a single waveform discontinuity.

A 1 kHz sine cannot expose a whole 480-frame repeat because that period contains
exactly ten cycles. The 997 Hz load signal and its analyser regression expose
that defect. The original 1 kHz idle measurement remains unchanged.

## Reproduced defects and changes

F1. Concurrent vCPUs timestamp chipset timer service before acquiring the clock
lock. An older caller can acquire the lock after a newer caller and move the
saved timestamp backward. The sequence 2 ms, 1 ms, 3 ms advances audio by 4 ms.
Clamping each timestamp to the previous timestamp makes the same test advance
exactly 3 ms. The regression also checks the ACPI PM timer.

F2. HDA waited for downstream playback before exposing IOC completion. Intel
HDA 1.0a specifies playback BCIS after the descriptor's last byte is fetched
into the DMA FIFO. Completion now follows DMA submission. LPIB follows the
local FIFO consumer, with lag limited to the advertised SDFIFOS+1 bytes.
Reporting the fetch cursor caused Windows to refill the active period.
Subtracting the entire staging queue also failed because that queue can exceed
the controller FIFO. Downstream graph delay belongs to neither IOC nor LPIB.
Queue occupancy still throttles submission, and deferred frame time remains
bounded. The `HDA1` snapshot layout and shared traits are unchanged.

The controller reference is Intel HDA 1.0a, SDnSTS.BCIS and SDLPIB:
https://www.intel.com/content/dam/www/public/us/en/documents/product-specifications/high-definition-audio-specification.pdf

Microsoft describes link position here:
https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/wall-clock-and-link-position-registers

F3. PCI MMIO held the global PCI-root lock throughout device service. Address
decode also locked unrelated PCI functions to read their command registers.
Slow BAR service blocked HDA register reads and interrupt acknowledgements.
The failing trace has 6,576 playback IOCs but only 3,026 acknowledgements.
The controller kept fetching stale guest periods while Windows could not refill
them. Host pulls stayed near 48 kHz, with no ring overwrite. This explains the
480- and 960-frame spacing behind the original 3,883 submitted spikes.

MMIO now resolves an owned target under the root lock, then releases that lock
before servicing the selected function. Command bits are cached and refreshed
after config writes and snapshot restore. A regression blocks a GPU BAR and
proves another PCI function remains accessible. It failed before this change.

F4. Slot-specific latency logs then identified virtio-blk notifications doing
synchronous disk I/O on the vCPU. A `QueueWorker` now services block queues on
a worker thread. Notifications coalesce per queue without losing queue indices.
Snapshot quiesce waits for prior work, pauses service, and resumes pending kicks
afterward. Capability reads are cached to avoid a transport interrupt-lock
inversion. The block state bytes and snapshot identifiers remain unchanged.
Tests cover blocked I/O, quiesce, resume, reset, and coalesced multi-queue kicks.
The PCI and block changes are separate commits because they fix blocking
defects outside the HDA model.

## Measured results

The final matched-source comparison retains the timer/completion changes in
the baseline but excludes the PCI, block-worker, and local-FIFO corrections:

| Measurement | Baseline submitted | Baseline monitor | Fixed submitted and monitor |
| --- | ---: | ---: | ---: |
| Active duration, s | 30.000, bounded window | 61.010 | 30.000 |
| Frequency, Hz | 1000.939 | 1000.279 | 997.000001 |
| Discontinuity spikes | 4,548 | 9,280 | 0 |
| Silent gaps of at least 1 ms | 0 | 2 | 0 |
| THD+N | +46.46 dB | +42.74 dB | -75.85 dB |

The baseline source takes 63,609 ms to submit the preroll, tone, and tail.
The fixed source takes about 31,234 ms. The same pinned native source,
silent preroll, and in-memory logging are used in both cases. Fixture changes
alone do not remove the defect.

Across three consecutive private-PipeWire repeats, absolute wall delivery error
is at most 0.000379%, below the 0.1% bar. Every repeat passes both captures.
Each trace has 3,124 playback IOCs and 3,124 acknowledgements, with 5,857 or
5,858 host pulls. Every pull receives its requested 256 frames. There are no
short pulls or ring overwrites. A separate traced pass has the same result.

The paced-backend load test also passes both captures, with zero spikes,
zero gaps, -75.85 dB THD+N, and +0.000087% wall delivery error.

Final idle Windows and Linux quality tests pass with zero spikes or gaps.
Windows measures -76.31 dB THD+N and -0.000200% wall delivery error.
Linux measures -81.81 dB and +0.000548%. Linux capture, microphone-off,
and snapshot playback pass with the asynchronous block worker enabled.

An earlier candidate run made concurrently with the failing baseline still
recorded four submitted spikes, ten monitor spikes, and one monitor gap.
The four subsequent isolated traced runs pass. The cause of that concurrent-run
failure was not established. These results qualify one loaded guest, not
simultaneous GPU-loaded guests or real-time host scheduling under arbitrary load.

Shared-mode experiments produced silent PCM in the agent session, including
after endpoint unmute. They are not acceptance evidence. Replacing the managed
source with the native source did not remove the exclusive-mode failure.

## Run the focused checks

Building the native load tool requires `x86_64-w64-mingw32-gcc`. The PipeWire
test also requires `pipewire`, `wireplumber`, `pw-dump`, and `pw-record`.
Select a DRM render node and a Windows win3d template explicitly:

```sh
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- env \
  VMLAB_SKIP_UPDATES=1 \
  VMLAB_HV_TEST_VENUS_RENDER_NODE=/dev/dri/renderD128 \
  VMLAB_HV_TEST_WINDOWS_DISK=/path/to/win3d/disk.qcow2 \
  VMLAB_HV_AUDIO_QUALITY_ARTIFACTS="$PWD/.scratch/hda-load" \
  cargo test -p vmlab-hv-tests --test audio_windows \
    windows_hda_pipewire_load_quality_via_s1 -- --ignored --nocapture
```

Replace the filter with `windows_hda_playback_load_quality_via_s1` to measure
the paced recording backend. Each VM receives its own short temporary XDG
runtime and state directories. Test children are killed and reaped on failure.
Artifacts preserve PCM, timing, guest-period CSV, and diagnostics outside the
disposable runtime.
`VMLAB_HV_TEST_AUDIO_BINARY` selects an instrumented baseline binary.

Set `VMLAB_HV_LOG` to
`info,vmlab_hv_devices::hda::period=debug,vmlab_hv_devices::hda::dma=debug,vmlab_hv_devices::audio::pipewire::pull=debug`
for BDL entries, DMA offsets, LPIB, IOC and ACK times, frames requested,
frames available, delivered frames, and silence decisions. Add
`vmlab_hv_devices::audio::pipewire::dma=debug` for each ring write and its
discard count, or `vmlab_hv_vmm::pci::latency=debug` for slow BAR service.
The broader `vmlab_hv_devices::hda=trace` also records per-tick deferred time.
Verbose logging adds overhead.

The full hypervisor `just ci::check` gate is intentionally omitted under the
owner's 2026-11-04 policy. The PR lists the exact focused checks.
