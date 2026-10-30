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
driver support in addition to EGL.

The process inherits the launcher's environment unchanged. This includes
Vulkan ICD/loader settings and the PCI filter layer supplied by vmlab. The
hypervisor does not translate `qemu_env` or choose a replacement ICD.

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

The renderer and EGL readback run on one worker thread. Each published frame
has a fresh dma-buf allocation; guest rendering cannot overwrite a frame held
by a client. External-texture sampling supports external-only imports, including
NVIDIA buffers. Rendering finishes before publication. Where available, an
acquire sync-file accompanies the frame. `CpuCopy` returns pixels from that
same immutable frame, including its stride padding.

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

Run VM tests under the work package's 24 GiB memory scope. The shared harness
limits concurrent test VMs to two. `just ci::check` remains the full merge gate.
