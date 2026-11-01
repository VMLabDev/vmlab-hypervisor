# virtio-snd capture timeout investigation

## Cause

The machine-events thread rendered display frames before advancing the
chipset and its audio clock. Frame publication took more than 100 ms in a
debug build under load. The guest continued running, but virtio-snd received
no clock calls and could not return a capture period during that interval.
ALSA timed out its blocking capture read and returned `EIO`.

The earlier fix, `0027ad2`, limits capture catch-up to one period after a
clock stall. That prevents completion bursts, but cannot prevent a timeout
while the clock is stalled. The fault is in the VMM's scheduling of the
device, rather than its PCM status or lifecycle handling.

## Reproduction and evidence

The baseline was branch `wp/vsnd-eio` at `25c3c82`. The guest was the
ubuntu-26.04 template, version `26.04.3`, with Linux `7.0.0-30-generic`.
The failing command was:

```sh
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  cargo test -p vmlab-hv-tests --test audio_virtio_guest \
  virtio_snd_guest_plays_and_records -- --exact --nocapture
```

The uninstrumented run failed at the capture read. ALSA reported `RUNNING`,
`hw_ptr=49920`, and `appl_ptr=49152`. Playback had succeeded. The host had
opened playback and capture at 48000 Hz, two channels, s16le.

Temporary device logging recorded control requests, responses, stream
states, capture completions, interrupts, and delayed clock calls. Temporary
VMM logging measured display publication. Dynamic debug was enabled for
`sound/core/pcm_lib.c` in the guest. These probes were removed after diagnosis.
The test retains guest `dmesg` collection when a command fails.

One failing trace showed this sequence. Times are host seconds since the
first virtio-snd control request:

| Time | Request or completion | Result |
| --- | --- | --- |
| 11.776821 | Capture SET_PARAMS, buffer 8192 bytes, period 1024 bytes | `S_OK`, state Set |
| 11.777192 | Capture PREPARE | `S_OK`, state Prepared |
| 11.777594 | Capture START | `S_OK`, state Started |
| 11.784899 to 11.831153 | Ten capture completions | Each `S_OK`, 1024 PCM bytes plus eight status bytes, each followed by an interrupt |
| 11.948549 | Next audio clock call | Elapsed 113.026225 ms |
| 11.948869 | Eleventh capture completion | `S_OK`, used length 1032 |
| 11.969124 | Next audio clock call | Elapsed 20.594141 ms |
| 11.979657 | Capture STOP during error cleanup | `S_OK`, state Stopped |
| 11.979979 | Capture RELEASE | Eight pending buffers cancelled with `S_IO_ERR`, then control `S_OK` |

The display publication surrounding the stall took 132.560973 ms. Other
publications in that run took 113 to 140 ms. The guest kernel recorded:

```text
[   24.063625] virtio_snd virtio2: capture read timeout (DMA or IRQ trouble?)
```

The failed read still showed `RUNNING`, `hw_ptr=3584`, and `appl_ptr=2560`.
There was no failed control request or PCM completion before cleanup.
The `S_IO_ERR` cancellations occurred after STOP and RELEASE, not before
the read failed.

Linux v7.0's `wait_for_avail()` uses a minimum 100 ms timeout. With a
2048-frame buffer at 48000 Hz, its buffer-derived timeout is smaller than
that minimum. A timeout returns `-EIO` even if a later interrupt makes PCM
available before userspace reads the diagnostic status.

Primary references:

- Linux ALSA wait and timeout handling: https://github.com/torvalds/linux/blob/v7.0/sound/core/pcm_lib.c
- Linux virtio-snd completion handling: https://github.com/torvalds/linux/blob/v7.0/sound/virtio/virtio_pcm_msg.c
- VIRTIO 1.2, section 5.14.6.8: https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html

Local raw logs are under `.scratch/vsnd-eio/`: `baseline.log`,
`status-trace.log`, `timing-trace.log`, `display-trace.log`, and
`clock-before.log`. They are investigation artifacts, not build inputs.

## Change

Display publication runs on the `display-frames` worker. The machine-events
thread continues to advance the chipset and audio independently of frame
rendering. The worker preserves display rate limiting, exits with the VM,
and is joined during normal and partial-construction cleanup.

Moving publication alone was insufficient. A later loaded run reproduced
capture `EIO` with ALSA reporting `XRUN`, `hw_ptr=44288`, and
`appl_ptr=42240`. Rendering still held the VGA device mutex and could block
guest VGA accesses. Common little-endian XRGB8888 scanout now copies rows
directly instead of calling the pixel translator for every pixel under
that mutex. The output still clears the unused X byte. Other formats and
partially out-of-range rows use the existing translator. A regression
compares the two paths with virtual stride, panning, and the VRAM boundary.

`slow_display_does_not_block_audio_clock` blocks a real frame publication
and observes audio clock calls from the running machine. It failed before
the change and passed afterward. The test attaches the display server after
VM construction, matching binary startup. Worker creation depends on the
configured video device, even when its server is not yet attached.

An initial implementation checked only for an already attached server.
That disabled periodic publication for real guests. Five completed runs
with that implementation were discarded, and the 30-run check restarted
after correcting startup. The full guest test then passed with publication
enabled, including playback, capture, and the online audio snapshot round trip.

The first loaded attempt with display publication enabled passed capture
but later failed the post-restore playback assertion. Only 706560 of the
required 1052160 PCM bytes reached the host. That run's background client
output had been redirected to `/dev/null`, so its exact failure reason was
not captured. The test now retains that output and reads PCM state and
`dmesg` if the dump stops growing. No assertion or retry policy changed.

Device regression tests also exposed these defects:

- F1. Time accrued with no transmit buffers completed eight newly posted
  periods immediately, rather than waiting for playback time.
- F2. A one-second clock stall completed eight queued periods in one call.
- F3. RELEASE returned only one of eight pending playback messages.
- F4. Restore omitted `tx_credit` and decoded `fraction` from its offset.
  The saved layout is capture credit, playback credit, then fraction.

Playback now discards credit when its queue is empty and limits catch-up
to one period, matching capture's existing pacing rule. RELEASE drains
every pending playback message before completing its control request.
The corresponding regressions failed before these changes and passed afterward.

## Post-restore timeout

An attempt reached 17 consecutive loaded passes before failing on run 18.
The retained background playback log reported `EIO` from `os.write` after
restore. ALSA remained `RUNNING`, with `hw_ptr=72960`, `appl_ptr=86400`, and
a 960-frame period in a 15360-frame buffer. Capture had succeeded.

KVM state was restored before device state. The restored KVM clock and
in-kernel timers continued advancing while host streams and other devices
were restored, even though no guest audio could progress. A temporary
600 ms delay during sound restore reproduced the playback `EIO` with
`hw_ptr=73920` and `appl_ptr=86400`. The playback wait timeout for this
buffer is 352 ms.

The implementation now refreshes captured KVM state after device restore
and diagnostic work. This restores the captured clock, TSC offsets,
in-kernel timers, and pending interrupt state together at the resume
boundary. The initial kernel restore remains because some devices depend
on its clock. The RTC worker quiesces during snapshots and adopts the
final clock anchor before it resumes.

`snapshot_load_excludes_device_restore_time_from_guest_clock` injects a
600 ms device restore delay and compares paired KVM clock/TSC samples.
It failed before the final refresh and passed afterward. The same controlled
600 ms sound-restore stall also passed the full guest audio test after the
refresh. The temporary stall injection was then removed. Both delayed TSC
rollback tests passed as well.

There are no changes to shared device traits, control protocol types,
snapshot formats, PCM status values, capture pacing, retries, or test thresholds.

## Verification

The final code passed 30 consecutive runs of
`virtio_snd_guest_plays_and_records`, with no skip or failure markers.
Each run booted a fresh Linux guest and covered playback, capture, and
the online snapshot round trip. The raw record is
`.scratch/vsnd-eio/proof-30.log`.

During all 30 runs, the build load repeatedly rebuilt `vmlab-hv-devices`
in release mode on cores 8 through 15 with a separate Cargo target directory.
VM processes inherit a 24 GiB memory cap and zero swap. The existing
`ManagedChild` guard checks host-wide capacity before each boot and limits
project VMs to four, excluding the Android emulator.

The full `just ci::check` gate passed with `CARGO_BUILD_JOBS=4` and
`RUST_TEST_THREADS=1`, under the same memory cap. Serial test execution
reduced VM concurrency on the shared host. The gate included all 171 VMM
unit tests, both virtio-snd guest tests, the S1 clock/snapshot tests, and
the other workspace tests. Its raw record is `.scratch/vsnd-eio/ci.log`.

These tests retained their existing default-ignore policy and did not run:

- I1. `s2_sequential_throughput_1gib`, the slow 1 GiB throughput baseline.
- I2. `virtio::block::tests::zero_fill_throughput`, the 4 GiB diagnostic.
- I3. `loopback_latency_against_real_pipewire`, which needs a live PipeWire session.

Real USB and VFIO passthrough checks were guarded because
`VMLAB_HV_TEST_USB_DEVICE` and `VMLAB_HV_TEST_VFIO_DEVICE` were unset.
No real PCI or USB device was selected. Those checks are not required to
validate this file-backed audio fix.

Shared interfaces and snapshot formats are unchanged. The branch is
`wp/vsnd-eio`. The work does not include a merge or push.
