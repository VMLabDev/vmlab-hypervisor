# Renderer reset contract and current gap

This entry concerns a virtio-gpu device reset during Windows TDR, not a guest
machine reboot. It documents the required backend contract and the gap in the
implementation tested at main `450de8c37bfc1235f9404308de1429e9bb037f5c`.
It does not implement a new renderer acknowledgement protocol.

## Required completion contract

Microsoft requires the KMD's reset callback to return only after GPU memory
access has stopped. The guest KMD waits for virtio reset completion before
releasing device-visible command storage and invalidating lost mappings.
The host must provide the corresponding completion guarantee.

https://learn.microsoft.com/en-us/windows-hardware/drivers/ddi/d3dkmddi/nc-d3dkmddi-dxgkddi_resetfromtimeout
https://docs.oasis-open.org/virtio/virtio/v1.2/virtio-v1.2.html

- C1: Stop accepting old-generation queue work and synchronize its CPU workers,
  interrupt publication, pending fences, and display consumers.
- C2: End old context/ring readers and native GPU access to reset resources
  before acknowledging success. Removing KVM aliases or sending destroy
  messages alone does not establish that barrier.
- C3: Preserve backing storage and ownership until those accesses have ended.
  Reset must not release memory that an old consumer can still access.
- C4: Propagate failure or timeout without acknowledging successful reset.
  A bounded failure path must retain resources that cannot yet be reclaimed.
- C5: Keep recovery local to the affected virtual device. A shared physical
  GPU reset or an owner VM restart is not an acceptable substitute.

No reset deadline is defined by the current implementation. Choosing and
enforcing one belongs to a future implementation of this contract.

## Current API boundary

The source inventory has four relevant boundaries; all four are described here.

| ID | Entry | Current result and side effects |
| --- | --- | --- |
| A1 | `VirtioPci::virtio_reset(&mut self)` | Returns no result. Clears transport state, then calls the device's reset method. |
| A2 | `Gpu::reset(&mut self)` | Returns no result. Logs a renderer reset error, then clears GPU state, queues and pending fences anyway. |
| A3 | `Renderer::reset(&self) -> Result<(), u32>` | Retires direct-display leases, then unreferences resources and destroys contexts on the renderer worker. Propagates local worker/teardown errors to A2. |
| A4 | `Worker::call` | Sends a closure to the worker and waits on `recv`. There is no receive deadline. Completion establishes that the local closure returned. |

A4 accepts a `Send + 'static` closure over `&mut Backend`, returning
`Result<T, u32>`, where `T: Send + 'static`. The other entries take no input
apart from their receiver. None has a configurable reset deadline or default.

Source locations at the tested revision:

https://github.com/VMLabDev/vmlab-hypervisor/blob/450de8c37bfc1235f9404308de1429e9bb037f5c/crates/vmlab-hv-devices/src/virtio/pci.rs#L450
https://github.com/VMLabDev/vmlab-hypervisor/blob/450de8c37bfc1235f9404308de1429e9bb037f5c/crates/vmlab-hv-devices/src/virtio/gpu/mod.rs#L900
https://github.com/VMLabDev/vmlab-hypervisor/blob/450de8c37bfc1235f9404308de1429e9bb037f5c/crates/vmlab-hv-devices/src/virtio/gpu/renderer.rs#L615
https://github.com/VMLabDev/vmlab-hypervisor/blob/450de8c37bfc1235f9404308de1429e9bb037f5c/crates/vmlab-hv-devices/src/virtio/gpu/renderer.rs#L95
https://github.com/VMLabDev/vmlab-hypervisor/blob/450de8c37bfc1235f9404308de1429e9bb037f5c/crates/vmlab-hv-vmm/src/pci.rs#L524

The PCI device mutex serializes BAR reads and writes. Although A1 sets status
zero before calling the device, another vCPU cannot observe that value through
the same PCI device while the synchronous reset call still holds its lock.
The gap is the meaning of completed teardown, not an observed early status read.

## Gap in the Venus backend

F1: A2 discards local teardown failure. The guest can observe reset completion
after an error. `Backend::unref` removes the BlobMapper alias before native
unmap/unref, but an error can stop teardown partway through.

F2: The pinned Rutabaga 0.1.80 component calls void virglrenderer resource and
context teardown APIs. In virglrenderer 1.3.0's Venus proxy, destroy requests
have no completion reply. Joining the local fence thread does not join server
ring threads. Process-worker destruction sends SIGKILL without synchronously
waiting for reap. The inspected graceful device teardown waits for GPU idle
only in the thread-worker path. Successful local calls therefore do not prove
C2 for the process-worker configuration used by these tests.

F3: Direct-display lease retirement and A4 can wait without a deadline. The
existing `Backend::synchronize` helper is not called by reset, and this audit
does not establish it as an idle barrier for every Venus Vulkan queue.

Relevant pinned dependency sources:

https://docs.rs/rutabaga_gfx/0.1.80/src/rutabaga_gfx/rutabaga_core.rs.html
https://docs.rs/rutabaga_gfx/0.1.80/src/rutabaga_gfx/virgl_renderer.rs.html
https://gitlab.freedesktop.org/virgl/virglrenderer/-/blob/1.3.0/src/proxy/proxy_context.c
https://gitlab.freedesktop.org/virgl/virglrenderer/-/blob/1.3.0/src/proxy/proxy_client.c
https://gitlab.freedesktop.org/virgl/virglrenderer/-/blob/1.3.0/server/render_worker.c
https://gitlab.freedesktop.org/virgl/virglrenderer/-/blob/1.3.0/src/venus/vkr_device.c

No guest-RAM use-after-free was demonstrated. Removed guest aliases and
FD-backed object lifetimes limit that memory-lifetime risk. This is not proof
of the stronger completion guarantee.

## Verification boundary

The accepted Windows package `0.1.3`, KMD `100.6.101.58104`, passed four fresh
native boots, twice each on NVIDIA and AMD. All 24 Vulkan and x64/x86 D3D11
readbacks passed before/after forced TDR, with both recovery counters advancing,
successful statuses, restored test policy, and no bugcheck or OS reboot.
Those functional results do not establish C2 or bounded failure handling.

The regression queries System BugCheck event 1001 and queued/archived WER
BlueScreen reports from first-boot polling through final rendering. OneDrive's
post-TDR `ucrtbase.dll` access violation was observed and deliberately not
pursued at the owner's request. It does not block the owner's acceptance bar.

The original unforced OOBE stall remains unattributed. INTx status-3 handling
is fixed in the guest KMD and software-tested across all ISR-byte values;
no native combined-status-3 coverage or original-stall cause is claimed here.
