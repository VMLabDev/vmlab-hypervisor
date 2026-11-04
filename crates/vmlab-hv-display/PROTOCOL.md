# Display protocol version 1

This reference covers all 13 `Message` variants in `src/protocol.rs`, the
native client, and the server integration API. The phase-2 specification's
2b section defines the fields and semantics. This crate assigns their widths,
kind numbers, and byte offsets. `tests/wire.rs` pins every variant against
literal wire bytes.

## Envelope

For q35 machines, `display.socket` and `display.vnc_socket` in the machine
description select the native and RFB Unix socket paths. Both are optional.
When absent, they default to `display.sock` and `vnc.sock` beside the control
socket. The paths must differ and must not already exist. The binary binds
both before resuming a guest, including when launched with `--paused`.
A connected native client receives the initial surface while paused and
subsequent firmware frames after resume.

The transport is an AF_UNIX stream. Each message starts with this header:

| Byte offset | Type | Meaning |
|---|---|---|
| 0 | u32 | Total length, including the 8-byte header |
| 4 | u16 | Kind from the table below |
| 6 | u16 | Frame flags for FrameReady when negotiated, otherwise zero |

All native integers are little-endian, without alignment or padding.
Signed integers use two's complement. Booleans are u32, either zero or one.
Offsets below are relative to the payload, after the header.
The maximum total message size is 16 MiB. Unknown kinds, flags on other messages,
invalid lengths, invalid booleans, and trailing bytes are errors.

Descriptors accompany the first header byte as `SOL_SOCKET/SCM_RIGHTS`.
`send` handles partial writes. `receive` handles fragmented headers, limits
descriptor counts, rejects truncated ancillary data, and returns close-on-exec
`OwnedFd`s. The caller serializes concurrent raw transport reads and writes.
`Client` serializes its own reads and writes with separate locks.

## Message layouts

### Hello, kind 1

Payload: `version: u32 @0`, `capabilities: u32 @4`. Total length 16. No fds.
The client sends this first. Version must be 1. The server replies with the
intersection of the requested capabilities and those it implements.
Unknown capability bits are ignored. A version mismatch closes the connection.

Capability bits are frames=1, cursor=2, input=4, dmabuf_ok=8, cpu_copy=16,
frame_flags=32, stable_buffers=64.
Frames gates surface and frame delivery. Cursor gates shape and state.
Input gates input acceptance and LED delivery. Dma-buf delivery requires
frames and dmabuf_ok. CpuCopy requires cpu_copy and a configured surface.

### Stable buffers extension, version 1

`stable_buffers` is an additive v1 capability. It changes no message layout.
With frames and dmabuf_ok, it lets the server reuse a dma-buf configure across
frames. Cache imports by `(surface_id, buffer)`, not by sequence number.
The configure announces the buffers in a fallback pool or the compatible
direct allocations encountered on this connection. A new allocation extends
the announced set and replaces the configure. Known allocations keep their
announced indices even when the source changes their order or selects a subset.
Geometry, format, modifier and transport changes also replace the configure.
A guest resource flip or source generation change alone does not.
The set holds at most 16 allocations. Adding beyond that limit replaces it
with the current source's buffer set.

Every FrameReady must be released, including frames without damage. Wait for
the acquire fence, finish all GPU sampling, then send Release. Release is the
completion token; no release fence fd is sent. The producer prevents writes to
a held allocation until every client releases it or disconnects. Clients must
not sample a buffer after Release, even if its configure is still cached.
Holding a direct guest buffer can stall guest rendering. Keep it only for the
GPU work needed to sample it, not until a later frame arrives.

Without stable_buffers the server keeps the original per-frame dma-buf
configure behaviour. Shared-memory clients need no new capability.

### SurfaceConfigure, kind 2

Payload: `surface_id: u64 @0`, `width: u32 @8`, `height: u32 @12`,
`drm_fourcc: u32 @16`, `stride: u32 @20`, `modifier: u64 @24`,
`kind: u32 @32`, `buffers: u32 @36`. Total length 48.

Kind is shm=0 or dmabuf=1. Exactly `buffers` fds follow in index order,
between 1 and 16. The server uses three memfds for shm. Shm is linear,
with modifier zero. Every buffer has `stride * height` bytes. Size seals
prevent truncation and growth. Fds support read-only client mmap.

A configure supersedes the previous configure. Surface IDs increase within
one connection and restart on reconnect. Old held frames remain valid until
Release. Stable configures can retain allocations from previous configures.
The producer cannot modify an allocation while any held frame leases it.
Shared memory uses XRGB8888 or ARGB8888. Stable dma-bufs also support
XBGR8888 (`XB24`) and ABGR8888 (`AB24`), with one plane at offset zero.
Multi-plane formats and nonzero plane offsets are not represented in v1.

### FrameReady, kind 3

Payload: `surface_id: u64 @0`, `buffer: u32 @8`, `seq: u64 @12`,
`timestamp_ns: u64 @20`, `damage_count: u32 @28`, then rectangles at @32.
Each rectangle is `x: u32`, `y: u32`, `width: u32`, `height: u32`, 16 bytes.
Total length is `40 + 16 * damage_count`.

When frame_flags is negotiated, header flag bit 0 marks a host placeholder.
It contains "Display output is not active." on a dark background. It is
available before the first resume and replaces guest output when no active
scanout exists, including after reset. A configured black guest scanout is
not a placeholder. RFB clients receive the same image without flag metadata.

Clients that distinguish guest paint from host output request frame_flags
and exclude placeholder frames. Unknown FrameReady flag bits are ignored.
The server sends zero header flags to clients without frame_flags. This
preserves compatibility with older v1 decoders that require zero flags.
The payload and protocol version are unchanged. A flag change publishes a
frame with full damage even if geometry and pixel colour are unchanged.

Buffer is a zero-based index in the current configure. Seq increases on each
publication. Timestamp is the source's monotonic time in nanoseconds.
Damage lies within the configured dimensions. A first frame, a reconfigure,
or skipped publications cause full damage. Shm pixels are complete even when
damage is partial. Empty damage leaves the displayed pixels unchanged.

Zero fds normally accompany a frame. A dma-buf frame may carry one acquire
sync_file fd. The client waits on that fence before accessing the dma-buf.
The client releases the frame only after its CPU or GPU work has finished.
Release carries no fence. Shm frames never carry an acquire fd.

### Release, kind 4

Payload: `surface_id: u64 @0`, `buffer: u32 @8`, `seq: u64 @12`.
Total length 28. No fds. Client to server.
The tuple must match a held frame, including frames from older configures.
A duplicate or unknown Release closes the connection.

### CursorShape, kind 5

Payload: `width: u32 @0`, `height: u32 @4`, `hot_x: u32 @8`,
`hot_y: u32 @12`, then `width * height * 4` pixel bytes at @16.
Total length is `24 + width * height * 4`. No fds. Server to client.

Pixels are premultiplied ARGB8888 words, bytes B G R A. Dimensions are at most
1024 each. Both dimensions must be zero for an empty shape. Nonempty shapes
require a hotspot within the shape. CursorState controls visibility.

### CursorState, kind 6

Payload: `x: i32 @0`, `y: i32 @4`, `visible: bool @8`.
Total length 20. No fds. Server to client. Coordinates locate the hotspot in
framebuffer pixels and may be negative.

### CpuCopy, kind 7

Payload: `surface_id: u64 @0`. Total length 16. No fds. Client to server.
The ID must name the latest configure. The server captures the current source,
performs synchronized CPU readback through `FrameSource::readback` for dma-buf,
sends a shm configure when needed, and publishes
a full frame. The connection then remains in CPU mode. Other clients retain
their independently negotiated transport. A readback failure closes only
the affected connection. Clients without dmabuf_ok receive CPU frames directly.

### Key, kind 8

Payload: `down: bool @0`, `qcode_length: u32 @4`, then qcode bytes at @8.
Total length is `16 + qcode_length`. No fds. Client to server.
The qcode is 1 to 64 ASCII letters, digits, or underscores, without a terminator.
Names follow vmlab's QEMU qcodes, such as `ret`, `ctrl_r`, or `bracket_left`.
The InputSink supplies guest keyboard translation and routing.

### PointerAbs, kind 9

Payload: `x: u32 @0`, `y: u32 @4`, `width: u32 @8`, `height: u32 @12`.
Total length 24. No fds. Client to server.
Coordinates are pixels relative to the supplied dimensions. InputSink owns
validation, scaling, and routing to the tablet.

### Buttons, kind 10

Payload: `mask: u32 @0`. Total length 12. No fds. Client to server.
Bits 0, 1, and 2 are left, middle, and right. The value is the complete held
button state. Remaining bits are available to the InputSink.

### Scroll, kind 11

Payload: `dx: i32 @0`, `dy: i32 @4`. Total length 16. No fds.
Client to server. Values are wheel steps, positive right and down.

### LedState, kind 12

Payload: `mask: u32 @0`. Total length 12. No fds. Server to client.
Bits 0, 1, and 2 are Scroll Lock, Num Lock, and Caps Lock. The core masks
other bits. Current LED state is sent to every newly connected input client.

### RequestSize, kind 13

Payload: `width: u32 @0`, `height: u32 @4`. Total length 16. No fds.
Requires the input capability. Dimensions must be in 1..16384; invalid sizes
close the connection. The InputSink forwards this preferred size to virtio-gpu.
The guest driver and modesetting client choose when to apply it. Standard VGA
retains its guest-programmed mode.

## Server and client API

`DisplayServer::new(source: Arc<dyn FrameSource>, sink: Arc<dyn InputSink>)`
returns `io::Result<DisplayServer>` after capturing and validating the first
frame. DisplayServer is cloneable. It has no KVM or device-crate dependency.

`FrameSource::frame(&self) -> io::Result<Frame>` returns an immutable snapshot.
`Frame` carries flags, generation, dimensions, stride, format, modifier, timestamp,
damage, and `Pixels::Shm(Arc<Vec<u8>>)`, immutable `Pixels::Dmabuf` descriptors,
or `Pixels::Leased` reusable descriptors.
Generation changes on reset or same-sized scanout replacement. Geometry
changes also force configure. The source increments generation before publishing
a reset frame, even when the reset does not change dimensions.
Stable dma-buf clients identify surfaces by layout and allocation Arcs instead
of generation. A fallback pool can serve several guest scanout resources.
Its buffers retain their identities across those resource flips. A reset that
publishes a placeholder or replaces the allocation set still reconfigures.

`FrameSource::readback(&self, frame: &Frame) -> io::Result<Vec<u8>>` supplies
linear pixels for dma-buf consumers. The backend waits on acquire and performs
GPU sampling/readback when direct mapping is unavailable, including NVIDIA
external-only allocations. The core checks the returned byte count.
Hardware-specific import and GPU execution belong to the FrameSource backend.

For immutable pixels, the producer retains each allocation without rewriting it
while any Arc to the published Frame exists. For dma-buf this includes consumer
completion. `Pixels::Leased` instead carries a buffer list, an index, an acquire
fence, a `BufferLease`, optional cached CPU pixels, and a lazy readback callback.
The server acquires a lease before lending an allocation and retains it through
Release or disconnect. The producer retires every publication of an allocation
before writing it again. Retirement rejects new readers and waits for held
leases. Cached CPU pixels and copied shm buffers do not retain producer leases.
`DisplayServer::publish()` captures the source and replaces the latest frame.
It acquires no client lock and performs no socket I/O. Publication errors leave
the previous frame intact. Source capture and publication are serialized so
concurrent requests cannot replace a newer capture with an older one.
`DisplayServer::publish_frame(Frame)` accepts an already captured device flush.
It does not call the source and may run while the device is locked. A source
capture that races with a newer publication cannot replace that publication.
`needs_cpu()` reports native shm clients, CpuCopy clients and RFB connections.
Native input-only and dma-buf-only connections do not request readback.

The q35 adapter publishes native virtio-gpu frames on RESOURCE_FLUSH and
SET_SCANOUT, even without clients. Timestamps mark host receipt of the guest
command, before capture and GPU synchronization. Damage is clipped from the
resource rectangle to the scanout. Only VGA compatibility paths poll at 16 ms,
using VGA dirty state and KVM's framebuffer dirty log to avoid unchanged pixels.
Native workers wait for publications and releases, without a delivery timer.

Each client retains at most three frames. Held buffers cannot be reused.
When no buffer is available, the worker waits while publications continue.
After Release, the worker sends the latest frame with full damage if it skipped
updates. Without stable_buffers, each dma-buf publication uses a new configure
and retains the old allocation until Release. A client that holds frames across
resize must still
release those frames before receiving more than three outstanding frames.

`DisplayServer::set_cursor(Cursor)` validates and publishes cursor state.
`DisplayServer::set_leds(u32)` publishes guest LED changes.
`InputSink::input(&self, Message)` receives the negotiated input messages.
The machine adapter routes keys to PS/2 and pointer input to the xHCI tablet.
Device input runs on a separate worker, preserving per-connection order.
The shared queue holds at most 256 pending input messages. Consecutive pending
PointerAbs messages coalesce to the latest position. Buttons, Scroll, Key, and
RequestSize messages end a coalescing interval and retain their order.
A full queue applies backpressure until space exists, without closing the
client or dropping a transition. Full-queue warnings occur at most once per
five seconds. Shutdown cancels producers waiting for space.
A blocked input handler does not delay Release while queue capacity remains.
Once transitions fill the queue, transport backpressure can delay subsequent
messages on that connection, including Release.

The built-in tablet also coalesces consecutive motion-only reports, retaining
the position at each button or wheel transition. Its queue holds at most
1024 reports. A full transition queue returns DeviceError::Io(WouldBlock)
without consuming the event. The machine adapter retries that event outside
controller locks until interrupt IN drains space or the machine exits.
Tablet warnings occur at most once per five seconds.

`serve_native(UnixStream)` and `serve_rfb(UnixStream)` run one connection until
disconnect or error. Each uses a separate input reader. The caller controls
binding, permissions, socket timeouts, and worker lifetime. Shutting down the
stream terminates both connection workers. A socket error cannot block publish.
Both capture the current source during connection setup before sending pixels.
Native connections without the frames capability do not capture the source.
`serve(UnixListener, rfb: bool)` accepts connections and starts one worker per
connection until the listener fails. It does not remove socket paths.

`Client::connect(path, Capabilities)` and `Client::from_stream(stream, caps)`
complete the handshake. `receive()` returns a `Packet` containing Message and
owned fds. `send(&Message)`, `release(surface_id, buffer, seq)`, and
`cpu_copy(surface_id)` send client messages. `stream()` permits timeout and
shutdown control. Direct reads or writes through stream must not race Client.
Client performs no automatic Release or fence wait.

Frames must have nonzero dimensions at most 16384 each, a stride of at least
four bytes per pixel, at most 256 MiB of storage, and at most 65535 damage
rectangles. Shm storage must match stride times height and have modifier zero.
An invalid frame or cursor returns InvalidData. Transport I/O errors propagate.
Unexpected message direction or missing capability closes that connection.

## RFB compatibility

RFB is big-endian and follows RFC 6143's version 3.8 handshake with security
type None. It serves Raw=0, DesktopSize=-223, RichCursor=-239,
AlphaCursor=-314, and LED-state=-261. Extensions are sent only when requested.
Raw is the baseline encoding. True-color 8-, 16-, and 32-bit pixel formats
support either byte order. Indexed color and older protocol versions are errors.

The initial format is 32-bit little-endian B G R x, depth 24. Raw converts
source pixels into the requested format, with unused bits zero. Requests are
clipped to the framebuffer. Incremental requests wait for a frame, cursor, or
LED change. Each response contains current full pixels for the requested region.
A resize sends DesktopSize. A client without DesktopSize disconnects on resize.
Clients request the new framebuffer after DesktopSize.

AlphaCursor takes precedence over RichCursor. Its payload is a big-endian Raw
sub-encoding followed by little-endian premultiplied ARGB words, matching
vmlab's `src/vnc.rs` consumer. RichCursor carries pixels in the negotiated
format, then MSB-first transparency bits with each row rounded up to a byte.
A hidden cursor has zero dimensions. LED-state carries one byte.

KeyEvent maps X11 keysyms to US physical qcodes, including modifiers,
navigation, F1-F12, and keypad keys. Unknown keysyms are ignored.
PointerEvent clamps coordinates to the framebuffer, forwards held buttons,
and translates wheel-bit rising edges into Scroll. Clipboard input is consumed
and ignored, with a 1 MiB limit. SetEncodings is limited to 256 encodings.

Sources: https://www.rfc-editor.org/rfc/rfc6143 and
https://github.com/rfbproto/rfbproto/blob/master/rfbproto.rst . The AlphaCursor
byte order follows the compatibility target `/home/wil/dev/vmlab/src/vnc.rs`.

## Verification boundary

The P3 tests exercise literal layouts, real Unix streams and memfds, damage,
resize, reconnect, held-buffer immutability, independent slow clients,
descriptor passing, CPU-copy selection, cursor, LEDs, input, and RFB. The RFB
tests include a separate Python client on a named socket.

Dma-buf tests use synthetic descriptors and a synthetic readback backend.
They prove transport and lifetime handling, not GPU import, sync_file waiting,
or NVIDIA rendering. Real guest frames, guest input round-trip, and vmlab
vision/record e2e require the machine and renderer integration packages.

`crates/vmlab-hv/tests/gpu.rs` exercises real virgl scanouts through S1, stable
imports, guest flushes, concurrent CPU frames, release backpressure and latency.
`VMLAB_HV_TEST_RENDER_NODE` selects a render node. `VMLAB_HV_TEST_SCANOUT=implicit`
selects a resource that requires the pooled fallback. The default probe uses
an explicit-layout scanout. Results and hardware limitations are in
`docs/display-export.md`.
The same S1 test alternates two scanout resources. A pooled flip chain keeps
one configure. A direct flip chain adds its second allocation once, then
keeps the combined configure. `VMLAB_HV_TEST_FLIP_SECONDS=30` extends this
measurement to at least 30 seconds. `VMLAB_HV_TEST_HV_BINARY` selects a
baseline executable for comparison.
