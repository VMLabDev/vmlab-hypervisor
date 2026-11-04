# HDA playback quality

## Cause and change

The controller at `ab097af` limits its playback queue to one 256-frame
callback period. It discards whole frames of elapsed time when that queue is
full. Timer ticks and host callbacks have independent phases, so a full queue
can lose frame time even when both clocks average 48 kHz. DMA, LPIB, and IOC
completion then advance too slowly. The host inserts silence when it runs out
of PCM. PipeWire can report zero graph errors while this happens.

The fix retains deferred frame time and allows two callback periods in the
playback queue. Transfers remain at most 256 frames. Deferred time is capped
at 8,192 frames to bound recovery after a long host stall. Stop, reset, and
stream errors clear whole-frame debt. Capture retains its elapsed-time pacing.
At that revision, position and completion interrupts still follow host consumption.
The subsequent load investigation changes this behavior and is recorded in
`hda-load-quality.md`.

No shared trait, protocol, dependency, or PCI register changed. The `HDA1`
snapshot layout is unchanged. Its existing `fraction` field now also holds
bounded whole-frame debt. Old snapshots remain readable. Older binaries reject
new snapshots that contain whole-frame debt through their existing validation.

The public controller reference is Intel HDA 1.0a, sections 3.3 and 4:
https://www.intel.com/content/dam/www/public/us/en/documents/product-specifications/high-definition-audio-specification.pdf

## Measurement

The opt-in S1 tests resume disposable guests through the control socket. Each
guest produces ten seconds of S16LE stereo at 48 kHz:

- The left channel is a 1 kHz sine with amplitude 6,000 before codec gain.
- The right channel contains ten 1 ms clicks, one second apart, starting at 250 ms.
- A 250 ms silent tail separates the signal from stream shutdown.

Windows uses exclusive, event-driven WASAPI with MMCSS and 100 ms source
buffers. Linux uses the ALSA kernel interface with an 8,192-frame source ring
and 512-frame periods. ALSA underruns fail the quality test instead of silently
restarting the source. These source buffers are separate from the host's
256-frame pull quantum.

The `testing` binary records every device-to-backend write and its monotonic
timestamp. A silent worker pulls 256 frames per 48 kHz deadline and records
the resulting PCM, including underrun silence. It prebuffers one period before
starting its clock. No workstation sound server or physical microphone is used.
The file handles stay open to reduce measurement overhead.

The analyser reports zero-crossing frequency, active duration, PCM frames per
wall-clock second, click spacing, sample jumps, silent gaps of at least 1 ms,
and THD+N. THD+N is the residual power after fitting a nominal 1 kHz sine,
cosine, and DC component, relative to fitted signal power. Sample jumps exceed
1.5 times the largest clean 1 kHz sine step. Smaller defects still contribute
to THD+N. Frequency on severely discontinuous output is only a diagnostic.

Acceptance requires speed within 0.1%, duration within 10 ms of ten seconds,
zero dropouts, zero discontinuities, ten correctly spaced clicks, and THD+N
below -50 dB. Raw timestamps independently detect slow DMA even when the
submitted PCM contains an exact 1 kHz tone. Rendered PCM must also pass.

## Before and after

Both runs use the same measurement backend and guest signal. The baseline is
`ab097af` rebuilt with only the test-only recording instrumentation. Windows is
the Windows 11 `26100.1742.10` win3d template. Linux is Ubuntu `26.04.3`.
All disks are disposable overlays. The host had other workloads active, so
these are not an exclusive idle-workstation or hardware loopback qualification.

| Guest and revision | Submitted wall span, s | Wall speed error | Rendered duration, s | Rendered frequency, Hz | Dropouts | Jumps | THD+N, dB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Windows, `ab097af` | 10.235862 | -2.30468% | 10.230771 | 1007.645901 | 24 | 1167 | +37.030 |
| Windows, fixed | 10.000233 | -0.00274% | 9.999979 | 1000.000000 | 0 | 0 | -76.312 |
| Linux, `ab097af` | 10.109686 | -1.08538% | 10.103896 | 1003.670921 | 54 | 536 | +13.679 |
| Linux, fixed | 10.000308 | -0.00350% | 9.999979 | 1000.000000 | 0 | 0 | -81.811 |

Submitted PCM has zero jumps and dropouts in all four runs. Baseline raw sine
frequency is exactly 1 kHz, but DMA delivers it slowly enough to cause gaps at
the host sink. This distinguishes the pacing defect from a negotiated-rate
mismatch. Both fixed guests also passed a preceding independent strict run.

The deterministic 10-second, 1 ms timer reproduction changes from 460,000
submitted frames and 1,248 short pulls to exactly 480,000 frames and zero short
pulls. Jittered-timer regressions also pass at 44.1, 48, and 96 kHz. Deferred
time survives snapshot restore and clears on stop and descriptor errors.

The initial polling WASAPI quality source produced intermittent guest-buffer
repeats under workstation load, even after the pacing fix. The quality source
now uses event-driven full-buffer writes. The existing quiet three-second
polling-driver smoke test remains and passes. The Linux quality source uses
strict underrun handling and a deeper source ring than that smoke harness.
These quality results do not qualify arbitrary low-latency guest clients.

## Run the opt-in checks

Run guest tests serially within the required memory scope. The tests use short
temporary directories, disposable overlays, and per-process XDG overrides.
They skip with named reasons when KVM, firmware, or templates are unavailable.

```sh
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- sh -ec '
  runtime=$(mktemp -d /tmp/hq.XXXXXX)
  trap '\''rmdir "$runtime"'\'' EXIT
  export XDG_RUNTIME_DIR="$runtime" XDG_STATE_HOME="$runtime" TMPDIR="$runtime"
  export VMLAB_SKIP_UPDATES=1
  cargo test -p vmlab-hv-tests --test audio_windows windows_hda_playback_quality_via_s1 -- --ignored --nocapture
  cargo test -p vmlab-hv-tests --test audio_guest linux_hda_playback_quality_via_s1 -- --ignored --nocapture
'
```

`VMLAB_HV_TEST_WINDOWS_DISK` and `VMLAB_HV_TEST_VM_DISK` select template paths.
`VMLAB_HV_AUDIO_QUALITY_ARTIFACTS` preserves PCM, rendered PCM, stream events,
and JSON-lines timing under one subdirectory per guest.
`VMLAB_HV_TEST_AUDIO_BINARY` selects an explicitly instrumented binary for
baseline comparisons. `VMLAB_HV_AUDIO_QUALITY_REPORT_ONLY=1` prints failing
baseline metrics without asserting acceptance. Do not set it for acceptance.

## Verification and limits

The focused verification commands are:

```sh
cargo fmt --all --check
cargo clippy -p vmlab-hv-devices -p vmlab-hv-vmm -p vmlab-hv-tests --all-targets --all-features -- -D warnings
cargo test -p vmlab-hv-devices --all-features hda:: -- --nocapture
cargo test -p vmlab-hv-devices --all-features audio:: -- --nocapture --test-threads=1
cargo test -p vmlab-hv-tests --test audio_guest -- --nocapture --test-threads=1
cargo test -p vmlab-hv-tests --test audio_windows windows_inbox_hda_starts_and_plays_via_s1 -- --nocapture
cargo test -p vmlab-hv-vmm --features testing audio:: -- --nocapture
cargo test -p vmlab-hv-vmm --features testing audio_clock_freezes_between_pause_and_resume -- --nocapture
```

Guest-bearing commands use the memory scope above. The 23 backend audio tests
pass against a private PipeWire daemon, a policy-only WirePlumber instance, a
null sink, and the private PipeWire PulseAudio server. Pulse capture explicitly
uses that null sink's monitor. The production backends' playback, format,
capture, disconnect, and thread-join checks pass. No physical device is claimed.

The S1 regression checks scripted microphone PCM, microphone-off behavior,
playback across snapshot save/load, and the Windows inbox driver. The HDA
register regressions retain INTx, FIFO-size, and LPIB alias coverage from #11.
Three analyser tests reject speed error, gaps, sample jumps, and slow delivery
of otherwise clean PCM.

The full hypervisor gate is intentionally omitted under the owner's
2026-11-04 focused-test policy. Physical speaker/microphone loopback and an
exclusive idle-host run are not checked. Recovery from stalls longer than the
8,192-frame debt cap can still lose audio by design.
