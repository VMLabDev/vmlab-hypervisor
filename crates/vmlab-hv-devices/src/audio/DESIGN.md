# Host audio backends

## Stream ownership and scheduling

Each playback or capture endpoint owns a worker thread and a lock-free
`SpscRing`. The device writes playback periods or reads capture periods without
calling the sound server, allocating memory, or taking a lock. Missing PCM becomes
zero-filled silence. Ring overflow retires whole oldest periods. The ring uses
atomic payload bytes, period-end metadata, and a post-copy compare-and-swap so
an overwrite cannot publish torn PCM to the consumer.

Each direction creates its connection, mainloop, and stream on its worker.
Capture starts only through `open_capture`; creating a host never opens a
microphone. The device caller enforces the machine's microphone opt-in.

PipeWire process callbacks run on the owning mainloop thread. `RT_PROCESS` is
deliberately absent: the local listener's Rust state and all stream access stay
on one thread. The loop waits up to 5 ms for events, but wakes immediately when
an event arrives. This does not guarantee real-time scheduling or the 20 ms
guest-to-speaker target. Measuring that target requires the A4 guest loopback
harness and a tuned host.

PulseAudio uses a nonblocking mainloop iteration followed by a 1 ms worker
sleep. Playback writes are limited to the server's writable space. Capture
handles both data and holes, then discards the server fragment. Work per
iteration is bounded. Neither backend waits on the server in the device path.

## PipeWire 0.10.1 requirements

The dependency is pinned to `=0.10.1`, behind the `pipewire` Cargo feature.
Enable its `v0_3_50` dependency feature. That includes `v0_3_49`, which exposes
`Buffer::requested()`, and enables `Time::buffered()` and
`pw_stream_get_time_n`. Building needs the PipeWire and SPA development headers,
pkg-config, and libclang for bindgen. Runtime needs libpipewire and a reachable
session server. PulseAudio needs libpulse development files to build and
libpulse at runtime, behind the `pulse` feature.

The 0.10 ownership API uses `MainLoopRc`, `ContextRc`, `connect_rc`, and
`StreamBox`. `AudioInfoRaw` serializes an exact interleaved format into a SPA
EnumFormat pod. S16LE and F32LE support mono and stereo, with explicit channel
positions. Playback respects the requested frame count and the mapped buffer's
capacity. Capture respects the chunk offset and size. The per-machine properties
are `node.name`, `node.description`, and `node.latency = 256/48000`.

One raw API is needed. The safe `Buffer` wrapper cannot set `pw_buffer.size`.
`ProcessBuffer` therefore uses the re-exported `pipewire::sys` types and raw
dequeue and queue methods. Its drop guard returns the buffer exactly once.
SPA `Data` remains a transparent wrapper over the mapped data header. No direct
`pipewire-sys` dependency or additional Cargo dependency is needed.

The installed public header `pipewire/stream.h` defines `pw_time.queued` as the
sum of queued `pw_buffer.size` values. It recommends frames for audio. This
backend sets that size in frames and converts queued frames, resampler-buffered
frames, and graph delay to PCM bytes. Graph delay first converts from the timing
report's fractional rate to the negotiated sample rate. The device-side ring
occupancy is added to playback latency. Capture latency reports unread ring
bytes. PulseAudio converts its positive stream latency from microseconds to
PCM bytes and adds playback ring occupancy.

`PlaybackSink::queued_bytes()` reports only ring occupancy. HDA uses it to
limit DMA submission, while position uses `latency_bytes()`. Graph latency can
remain nonzero with an empty ring, so using it to throttle submission would
stall playback.

`queued_bytes()` reports only playback ring occupancy, or zero after the worker
exits. Device submission throttling uses this count rather than total latency.
Graph latency can exceed one period even with an empty ring. Tests pin this
distinction for both backends.

## Startup, failure, and teardown

Opening a stream waits for readiness outside the device's period path, with a
3-second deadline. PipeWire treats paused as ready because an unlinked node can
legitimately wait for a session manager. Playback smoke tests require actual
process callbacks, so readiness alone cannot pass them. PulseAudio waits for
both context and stream readiness and disables server autospawn.

Drop sets an atomic stop flag and joins the worker. PipeWire checks core errors,
stream errors, and loop errors. PulseAudio checks context and stream failures.
After a worker exits, playback writes discard PCM and playback latency is zero.
Capture can drain buffered PCM, then supplies silence. No worker reconnects
implicitly. Dropping all listeners, streams, cores, and contexts happens on their
owning worker threads. Global PipeWire deinitialization is omitted because other
machines or endpoints may still use the library.

## Verification

Run backend checks with `cargo check -p vmlab-hv-devices --all-features --all-targets`.
Run audio tests with `cargo test -p vmlab-hv-devices --all-features audio:: -- --nocapture`.
Real playback tests exercise both encodings and channel counts against the host
server. They print a named skip reason if the server socket is unavailable.
The PulseAudio test also opens capture streams and verifies worker joins.

The PipeWire disconnect test starts an isolated daemon under a `mktemp -d`
runtime directory. A subprocess opens a playback stream. The test kills only
that daemon, then asserts that the worker exits and endpoint drop joins promptly.
Subprocess environment overrides keep the running host session unchanged.
The test skips with a reason if the PipeWire executable is unavailable.

These tests prove the host-audio implementation below S1. They do not prove
guest PCM correctness, snapshot continuity, microphone opt-in wiring, or
guest-to-speaker latency. Those belong to A1 through A4 in the audio spec.
