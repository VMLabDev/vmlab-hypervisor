# Accelerated virtio-vga

## Machine configuration

The `Display` configuration in `vmlab-hv-proto/src/machine.rs` adds two
optional fields. They must be specified together on a `virtio-vga` display.

| Field | Values | Absence |
| --- | --- | --- |
| `gpu` | `virgl`, `venus` | Software 2D device |
| `render_node` | DRM render-node path, including a symlink to one | Required with `gpu` |

Example display section for a q35 machine:

```json
{
  "device": "virtio-vga",
  "gpu": "virgl",
  "render_node": "/dev/dri/renderD130",
  "pci": {"bus": 0, "device": 4, "function": 0},
  "rom": null
}
```

The render node must be readable and writable. Initialization fails rather
than accepting rutabaga's software fallback. GPU settings on `std-vga`, or a
mode without a render node, fail machine validation.

Virgl mode advertises virgl and virgl2 capsets. Venus mode adds the Venus
capset. Both modes support context creation, resource attachment, 3D
submission, transfers, fences and blob resources. Venus needs host Vulkan
driver support in addition to EGL. It enables virglrenderer's render server
and external blobs. Initialization rejects an empty Venus capset, even when
rutabaga reports the capset ID.

The process inherits the launcher's environment unchanged. This includes
Vulkan ICD/loader settings and the PCI filter layer supplied by vmlab. The
hypervisor does not translate `qemu_env` or choose a replacement ICD. The
vmlab launcher must apply the selected host GPU's environment itself, including
`RENDER_SERVER_EXEC_PATH` when a patched virglrenderer render server is needed.

## Host dependencies

`vmlab-hv-devices/Cargo.toml` pins `rutabaga_gfx = 0.1.80`, matching crosvm
revision `ad804a6ecf1d3c4337f18b56092fc54d8ca574fa`. `magma-gpu` is also
pinned to 0.1.80 because 0.1.85 removed an API required by that rutabaga
release. `build.rs` requires pkg-config entries for virglrenderer >= 1.3.0,
EGL and GBM. These are native dependencies, not bundled libraries.

## Memory and display

The accelerated function places MSI-X in BAR2 and exposes a 4 GiB,
64-bit prefetchable host-visible blob region in BAR4. Shared-memory
capability ID 1 describes that aperture. The VMM's `gpu::Aperture` registers
and removes blob mappings in KVM's normal and SMM address spaces. Overlapping,
unaligned and out-of-aperture mappings fail.

The renderer and EGL readback run on one worker thread. Simple scanout-bound
resources get an explicit-layout allocation before guest rendering begins.
The layout must support Vulkan import on the selected render node. The guest
renders directly into that allocation; display publication does not blit it.
Unknown layouts, cropped or multi-plane resources and external-only imports
use a three-buffer EGL blit pool. Pool allocation also prefers Vulkan-importable
modifiers. No CPU readback runs without a native shm, CpuCopy or RFB consumer.
An inferred GBM modifier is accepted only after EGL imports it successfully.
Otherwise a Vulkan blob retains its implicit layout for the blit path.

RESOURCE_FLUSH and SET_SCANOUT publish frames with guest damage. Only VGA
compatibility still polls. Native clients negotiate `stable_buffers` to cache
imports. A client retains a buffer lease until Release or disconnect. Guest
rendering waits for all leases on a direct scanout; pool buffers cannot be
overwritten while sampled. Renderer fences finish preceding work before a
frame is published. The guest fence timeline is separate from export fences.
`CpuCopy` returns the published frame, including its stride padding.

See `display-export.md` for the hardware checks and latency measurements.

Online snapshot save and restore return `unsupported` whenever acceleration
is attached, following phase-2 decision D9. Offline disk snapshots are unchanged.

## Verification

`display::virtio_vga::tests::accelerated_protocol_fences_transfer_and_immutable_scanout`
exercises PCI capabilities, capsets, contexts, 3D resources, submission,
asynchronous fences, transfers, guest-backed blobs, immutable frames and
malformed commands. Set `VMLAB_HV_TEST_RENDER_NODE` to run it against hardware.
`VMLAB_HV_TEST_GPU_MODE=venus` also checks the Venus capset.

`gpu::tests::blob_slots_map_relocate_reject_overlap_and_unmap` exercises real
KVM registrations without booting a guest.

The S1 test `ubuntu_virgl_scanout_and_cpu_copy` boots an overlay on the local
Ubuntu 26.04 template, checks the guest's capset log through the agent, renders
a virgl color-buffer clear through the guest DRM interface, receives a dma-buf,
requests `CpuCopy`, and checks screenshot pixels. It also checks online snapshot
refusals. No guest packages are installed. The template defaults to the newest
local version; `VMLAB_HV_TEST_VM_DISK` overrides it. The test skips with a reason
if KVM, the template, firmware or a usable render node is missing.

The S1 test `steamos_venus_vulkan_device_and_frames` boots a private SteamOS
overlay, requires `vulkaninfo --summary` to report `DRIVER_ID_MESA_VENUS`, and
runs gamescope with a Wayland vkcube client. It requires five non-placeholder
dma-buf frames through the native display protocol and a successful 120-frame
vkcube run using Mesa's Wayland WSI. Set
`VMLAB_HV_TEST_VENUS_RENDER_NODE` to enable it and
`VMLAB_HV_TEST_VENUS_DISK` to override the SteamOS 3.8.14.3 template. OVMF
comes from vmlab's local guest firmware directory. The launcher environment
must select the same Vulkan physical device as the render node, as vmlab's
driver filter and PCI filter do in production.

On 2026-11-02 this S1 test passed on both the NVIDIA RTX 5090 at
`/dev/dri/renderD128` and the AMD Ryzen 9 9950X integrated GPU at
`/dev/dri/renderD130`. Each run exposed one `DRIVER_ID_MESA_VENUS` device,
published five dma-buf frames and completed the 120-frame vkcube run. NVIDIA
used the host's patched `RENDER_SERVER_EXEC_PATH`; AMD used the system render
server. One earlier AMD boot timed out before the agent handshake; the
unchanged test passed on the next run.

Run VM tests under the work package's 24 GiB memory scope. The shared harness
checks the host-wide four-project-VM limit before booting. `just ci::check`
remains the full merge gate.
