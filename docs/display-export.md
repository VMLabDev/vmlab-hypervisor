# Scanout export and hardware checks

## Implementation

The v1 `stable_buffers` capability uses the existing SurfaceConfigure,
FrameReady and Release layouts. A client caches imports by surface and buffer
index. The server configures an allocation once and reuses its index. Clients
without the capability retain the per-frame dma-buf configure behaviour.
The wire contract is in `crates/vmlab-hv-display/PROTOCOL.md`.

Simple scanout-bound 3D resources use an explicit-layout GBM allocation,
imported into virglrenderer before the guest attaches or renders to the
resource. This avoids the implicit modifier that GL texture export can report.
Publication exports that resource directly, without a blit. Cropping,
unsupported layouts and external-only resources use a three-buffer EGL pool.
The source is sampled through GL_OES_EGL_image_external. Pool slots retain
their allocation until resize and cannot be overwritten while sampled.

`virtio/gpu/modifiers.rs` queries the Vulkan physical device matching the
selected render node's major and minor numbers. It selects one-plane modifiers
with importable dma-buf memory for sampled/transfer images. GBM allocation
prefers those modifiers for both guest scanouts and fallback pools. If Vulkan
query or explicit allocation is unavailable, ordinary GBM allocation remains
available; a viewer may then need CpuCopy. No physical-device binding or host
modesetting is involved.

Each publication has a BufferLease. Direct rendering waits for every held
publication of the allocation, not just the latest sequence. Releases and
disconnects drop sampling guards. A full fallback pool retains pending damage
and retries after a slot becomes free, without requiring another guest flush.
Shutdown disconnects display clients before joining vCPUs that may be waiting
for a release.
Device input uses a bounded worker queue so a resize waiting for GPU state
cannot block the protocol reader from accepting a buffer Release.

RESOURCE_FLUSH publishes clipped guest damage. SET_SCANOUT publishes a resource,
geometry or paint change; an identical SET_SCANOUT without intervening writes
does not recapture unchanged pixels. VGA compatibility alone keeps the 16 ms
poll. Export fences have host IDs separate from guest IDs, including coalesced
timeline completions. Frame timestamps precede renderer capture and synchronization.

CPU readback is requested only by native shm, CpuCopy or RFB clients. Requested
readback runs on the renderer behind export, with its own sampling guard and
cached result. Dma-buf publication precedes that readback. Shm consumers receive
complete linear pixels and do not retain a producer lease through Release.
Placeholder flags and the host placeholder image are unchanged.
Returning to VGA clears the native-frame cache and lets the legacy poll inspect
the restored VGA state, avoiding a false placeholder between active frames.
CPU device frames convert XB24/AB24 channel order to their declared XRGB layout.

## Shared APIs

| API | Contract |
| --- | --- |
| `Pixels::Leased` | Carries reusable fds, buffer index, acquire fence, lease and protected readback callback. Existing Shm and immutable Dmabuf variants remain. |
| `BufferLease` / `BufferGuard` | Acquire before sampling; retire before reuse. Dropping a guard signals completion. `try_retire` leaves a held allocation unavailable to the pool. |
| `DisplayServer::publish_frame` / `needs_cpu` | Publish an already captured frame without re-entering the device; query whether any CPU consumer is connected. |
| GPU `DisplaySink` / `ScanoutUpdate` | Carries native scanout, damage, generation and command timestamp from the device to the VMM. Callbacks run with device state locked and must not call back into that device. |
| `Gpu` and `VirtioVga` display methods | `set_display_sink` registers the callback; `poll_scanout` identifies VGA compatibility paths. |
| `DmaFrame` | Adds reusable buffer metadata and a publication lease. `pixels` is optional; `readback()` obtains protected CPU pixels and returns WouldBlock for an expired uncached publication. |

The control protocol and the display protocol's message layouts are unchanged.

## S1 hardware results

Recorded on 2026-11-02 with the Ubuntu 26.04 template, using only render nodes:

| GPU | Node | Direct and pool output modifier |
| --- | --- | --- |
| AMD Ryzen 9 9950X integrated GPU | `/dev/dri/renderD130` | `0x200000000401b03` |
| NVIDIA GeForce RTX 5090 | `/dev/dri/renderD128` | `0x300000000606015` |

`crates/vmlab-hv/tests/gpu.rs` boots the guest through S1, renders a virgl colour
buffer, requests repeated DRM flushes, and connects a concurrent shm client.
It checks stable surface IDs and buffer imports, damage, correct CPU pixels,
zero readback before the CPU client connects, and CpuCopy compatibility.
The direct case proves guest writes wait for a held frame. The pool case holds
all three slots, changes guest paint, and proves that a release delivers the
latest paint without another guest flush. No guest packages are installed.

The explicit scanout and deliberately unexportable GL scanout run separately.
Counts below cover exports after the initial connection, including the release
checks and restored paint. They are scenario counts, not desktop workload rates.

| GPU | Scanout | Direct exports | Pooled exports | Fallback rate |
| --- | --- | ---: | ---: | ---: |
| AMD | Explicit layout | 18 | 0 | 0% |
| AMD | Implicit GL layout | 0 | 20 | 100% |
| NVIDIA | Explicit layout | 18 | 0 | 0% |
| NVIDIA | Implicit GL layout | 0 | 20 | 100% |

Boot-time console exports are excluded above. In these runs, the explicit
AMD guest had 2 direct and 22 pooled exports before measurement; the explicit
NVIDIA guest had 2 direct and 14 pooled exports. The implicit AMD and NVIDIA
guests had 25 and 24 pooled boot exports respectively. All had zero CPU
readbacks before a CPU consumer connected. Debug `scanout_export` JSON records
expose cumulative direct, fallback and readback counts per render node.

The NVIDIA guest fixture emits modifier `0x300000000e08014`, which EGL can
import but this Vulkan driver cannot. It is not marked external-only. A separate
real-GPU test, `external_only_scanout_reuses_pool_and_keeps_held_gpu_pixels`,
allocates a linear modifier-0 buffer which NVIDIA marks external-only. It paints
through GBM mapping, samples through the same export pool, checks held GPU pixels
after source changes, and proves slot/fd reuse after release. It passes on the
RTX 5090 with pool modifier `0x300000000606015`. This is GPU-level evidence;
the current virgl S1 fixture does not itself produce an external-only source.

## Flush-to-client latency

Measurements use CLOCK_MONOTONIC at host receipt of the guest GPU command and
immediately after a reader receives FrameReady. The reader waits concurrently
with the agent request, so agent RPC completion is outside the timing path.
Each entry contains eight samples. Values are median / maximum milliseconds.

| GPU | Export | Dma-buf alone | Dma-buf with shm client | Shm client |
| --- | --- | ---: | ---: | ---: |
| AMD | Direct | 0.464 / 0.680 | 0.464 / 0.715 | 22.341 / 27.350 |
| AMD | Pool | 1.749 / 1.935 | 1.579 / 1.866 | 21.071 / 21.604 |
| NVIDIA | Direct | 0.318 / 0.336 | 0.631 / 1.530 | 21.708 / 22.665 |
| NVIDIA | Pool | 1.085 / 1.593 | 1.496 / 1.875 | 21.603 / 22.731 |

These numbers measure export and socket delivery, not monitor presentation.
CPU readback remains substantially slower than dma-buf delivery. This fixture's
kernel expands legacy DIRTYFB clips to full-plane RESOURCE_FLUSH rectangles;
S1 checks those actual full rectangles. Device and native-server tests separately
check partial damage and clipping. A Venus command-stream workload and physical
display latency were not measured here.

## Reproduction and compatibility

Run the S1 probe from the repository root. The shared ManagedChild harness
checks host-wide VM capacity before each boot and removes its temporary guest
directory when done. Keep the workload inside the memory scope:

```sh
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  env VMLAB_HV_TEST_RENDER_NODE=/dev/dri/renderD130 \
  cargo test -p vmlab-hv --test gpu -- --nocapture --test-threads=1
```

Set `VMLAB_HV_TEST_SCANOUT=implicit` for the pool case. Repeat with
`/dev/dri/renderD128` for NVIDIA. An absent KVM device, usable render node,
firmware or Ubuntu template produces a named skip. `VMLAB_HV_TEST_VM_DISK`
overrides the template. The probe prints a `S1_export` JSON record with counts,
the modifier, and raw latency samples.

The nine existing tests in `/home/wil/dev/vmlab/src/hvdisplay/tests.rs` pass
against this branch's display crate. They compile the unchanged vmlab
`hvdisplay`, `vnc`, and their test modules in an isolated local Cargo package.
They cover RGB screenshots, placeholder exclusion, recorder damage/cursor/resize
events, releases, disconnects, and input reconnects. The vmlab checkout and its
lockfile are unchanged. Display crate tests also exercise real memfds and an
independent Python RFB client.

Focused regressions cover resize-before-Release, held pool pixels, 32-bit
renderer fence-ID exhaustion, CPU colour ordering, and VGA restore without a
false placeholder. The S1 direct case also sends RequestSize while holding the
frame, before Release, on both GPUs.

## Source contracts

- VIRTIO 1.2 GPU commands: https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html
- dma-buf layout and implicit modifiers: https://dri.freedesktop.org/docs/drm/userspace-api/dma-buf-alloc-exchange.html
- GBM allocation and import declarations: `/usr/include/gbm.h`.
- virglrenderer resource import and export-query ABI: `/usr/include/virgl/virglrenderer.h`.
- Vulkan DRM modifier and external-image queries: https://registry.khronos.org/vulkan/specs/latest/man/html/VK_EXT_image_drm_format_modifier.html
- Linux DRM damage handling: https://raw.githubusercontent.com/torvalds/linux/master/drivers/gpu/drm/virtio/virtgpu_plane.c

The Rust additions use these public interfaces. No QEMU source was copied or
translated.
