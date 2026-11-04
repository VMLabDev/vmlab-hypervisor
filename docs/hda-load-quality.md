# HDA playback under load

The 30-second Windows load acceptance test still fails. The changes described
here do not resolve the owner's music-playback report. Do not treat the passing
idle tests as load qualification.

## Reproduction

The opt-in S1 tests boot disposable Windows 11 win3d overlays. A native WASAPI
client generates a 997 Hz left-channel sine and right-channel clicks once per
second. Exclusive event-driven playback requests 480-frame periods at 48 kHz.
MMCSS schedules the audio thread. Three below-normal-priority native workers
keep all but one of the four guest CPUs busy. A separate D3D11 hardware process
repeatedly clears a 1920 by 1080 render target and waits for GPU completion.
The test requires an explicitly selected render node and a win3d template.

The signal lasts 30 seconds, followed by 250 ms of silence. The native client
avoids managed allocations and records its capacity, callback gaps, and elapsed
time. GPU readiness and completed batch counts are checked. All work runs inside
the disposable guest. No PCI or USB device is claimed.

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

F2. HDA waits for downstream playback before exposing IOC completion. Intel
HDA 1.0a specifies playback BCIS after the descriptor's last byte is fetched
into the DMA FIFO. Completion now follows DMA submission. LPIB reports progress
across the emulated codec link instead of subtracting sound-server buffering.
Backend queue occupancy still throttles submission, and deferred frame time
remains bounded. The `HDA1` snapshot layout and shared traits are unchanged.

The controller reference is Intel HDA 1.0a, SDnSTS.BCIS and SDLPIB:
https://www.intel.com/content/dam/www/public/us/en/documents/product-specifications/high-definition-audio-specification.pdf

Microsoft describes link position here:
https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/wall-clock-and-link-position-registers

F3. These fixes do not explain or eliminate the remaining repeats. A native
source with CPU and hardware D3D11 load still takes about 56 seconds to submit
30.25 seconds of requested audio. Spikes cluster around 480- and 960-frame
distances, with adjacent spike pairs. Host delivery itself remains near 48 kHz.
Further work must distinguish driver notification loss from late guest refill.

## Measured results

The latest clock-fixed load run, before cosmetic harness cleanup, reports:

| Measurement | Submitted | Private PipeWire monitor |
| --- | ---: | ---: |
| Active duration, s | 30.000, bounded measurement window | 55.670 |
| Frequency, Hz | 999.937 | 999.515 |
| Speed error relative to 997 Hz | +0.2946% | +0.2523% |
| Discontinuity spikes | 3,883 | 7,122 |
| Silent gaps of at least 1 ms | 0 | 2 |
| THD+N | +42.20 dB | +44.27 dB |

Submitted delivery speed error is +0.000136%, but the PCM content repeats.
The native source reports 480-frame capacity, three CPU workers, 56,032 ms
elapsed, and a maximum callback gap of 63 ms. These results fail the requested
zero-discontinuity, 0.1% speed, and below -50 dB THD+N bars.

The instrumented `91ad438` baseline also fails with the native source and the
same CPU/GPU workload: 6,416 submitted spikes and 11,898 monitor spikes.
Independent runs on a shared workstation do not establish the size of any
improvement. Both revisions fail acceptance.

The final paced-backend load check also fails, with 3,989 submitted spikes,
8,327 rendered spikes, and a 62.400-second rendered signal. This rules out
PipeWire as a prerequisite for reproducing the failure.

Idle Windows still passes with zero discontinuities, -76.31 dB THD+N, and
-0.00120% wall delivery error. Linux passes with zero discontinuities,
-81.81 dB THD+N, and -0.00959% wall delivery error. Linux capture,
microphone-off behavior, and snapshot playback also pass. Those guest results
include the monotonic-clock correction.

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
Artifacts preserve PCM, timing, and diagnostics outside the disposable runtime.
`VMLAB_HV_TEST_AUDIO_BINARY` selects an instrumented baseline binary.

Use `VMLAB_HV_LOG=info,vmlab_hv_devices::hda=trace,vmlab_hv_devices::audio::pipewire=trace`
for per-tick DMA position, descriptor, deferred frames, queue occupancy, IOC,
and host pull diagnostics. Trace logging adds overhead.

The full hypervisor `just ci::check` gate is intentionally omitted under the
owner's 2026-11-04 policy. The draft PR lists the exact focused checks.
