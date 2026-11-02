# vmlab-hv-viewer

A Linux native viewer for the hypervisor's local display socket. The headless
`Core` handles the protocol, damage, input, and resize. The shell uses winit and
Vulkan through ash. Audio stays in the hypervisor.

## Run the viewer

Build with `cargo build -p vmlab-hv-viewer`. The host needs a Vulkan loader and
graphics ICD, libwayland-client, libxkbcommon, and the X11 libraries. Hardware
import tests also link libgbm, already used by the device crate.

```sh
vmlab-hv-viewer --display /path/to/display.sock \
  --lab-socket /path/to/lab.sock --machine desktop
```

`--display` is required. Supply `--lab-socket` and `--machine` together to enable
text clipboard synchronization. The socket must belong to the machine's lab
daemon. The machine name is local to that daemon, without a lab prefix.

Click to grab keyboard and pointer. Press left Ctrl+Alt to release. The title
shows the grab state, release chord, and the guest's Num, Caps, and Scroll Lock
indicators. These indicators mirror protocol LED state. The compositor still
owns physical host keyboard LEDs. Losing focus releases held guest keys and
buttons. X11 grabs use winit's Xlib connection. Wayland requires pointer
constraints and keyboard-shortcuts-inhibit support from the compositor.

The viewer keeps the host cursor visible until the guest supplies a nonempty
cursor shape. If confinement is unavailable and the pointer must be locked,
it draws a local arrow instead. Once a guest shape exists, the guest controls
cursor visibility, including explicit hide requests. Chord detection tracks
physical keys before and during a grab. The completing chord key stays local;
only keys forwarded to the guest receive matching releases.

Use `--release-chord ctrl_r+alt_r` to choose another chord. Chord components are
QEMU key names, joined by `+`. Both modifiers in the default chord are left-side
keys. Keyboard mapping follows physical evdev positions, including the main
keyboard, keypad, international keys, F1-F24, and media/navigation keys that the
QEMU vocabulary represents. Keys without a QEMU name are ignored. Host shortcuts
reserved by the compositor may remain unavailable.

Use `--shm` to disable dma-buf negotiation. WSLg detection also selects this
mode when `/mnt/wslg` exists or `WAYLAND_DISPLAY` is under that directory.
Vulkan is still required for shared-memory presentation.

Resize requests follow the window after 200 ms without another size change.
Guests decide whether to apply them. Guests with fixed VGA modes are letterboxed.
Pointer coordinates use the same viewport as rendering and clamp at its edges.

Clipboard synchronization runs only while focused. Host text is sent on focus
gain and when a 100 ms selection check detects a change. Guest text is polled
once per second. Host changes win when both selections changed between polls.
Clipboard errors are printed without interrupting display or input.

## Frame ownership and rendering

The viewer requests frames, cursor, input, frame flags, stable buffers, and CPU-copy support.
Dma-buf is requested unless shared memory was selected. Imports are cached by
surface and buffer index, with the DRM modifier and plane pitch from configure.
Unimportable formats or modifiers trigger CpuCopy on this connection.

Acquire fences are waited before reading a frame. Vulkan acquires foreign queue
ownership, samples the import into the swapchain, and returns ownership before
the frame's Release. A GPU fence completes before Release is sent. An owned
GPU repaint image allows exposes and fixed-mode resizes after releasing the
guest allocation. This adds a device-local image copy, without CPU readback.
Shared-memory uploads pack only damaged rectangles into staging buffers. The
first frame after configure initializes the whole image. Cursor pixels are
premultiplied ARGB and are composited at the protocol position and hotspot.

The swapchain uses MAILBOX when available, otherwise FIFO with two images where
the surface permits it. `VMLAB_VIEWER_PRESENT_MODE=fifo` forces FIFO for testing.
Only one GPU submission is in flight. Physical display latency still needs the manual check
below. No timing claim is inferred from a passing unit test.

## Run automated checks

Run `cargo test -p vmlab-hv-viewer -- --nocapture` for fake-server tests, an
offscreen Vulkan damage test, a private-Xvfb executable test, and the S1 guest
test. The window test never uses the owner's display or clipboard. Missing
Vulkan, Xvfb, KVM, firmware, or guest assets produce named skips where relevant.
S1 uses the Ubuntu template discovered by the existing template convention, or
`VMLAB_HV_UBUNTU_26_04`. `VMLAB_HV_BINARY` selects an already-built hypervisor.
It checks input through the agent, including ISO, F13, and volume keys, and
applies an 800x600 DRM mode. VMs use the
host-wide VM permit and a systemd scope with 24 GiB RAM and no swap.
The permit defaults to four VMs; `VMLAB_HV_TEST_VM_LIMIT` overrides it.
Disposable overlays and sockets are removed at test exit.

The Windows S1 test discovers the Windows 11 standard template, or uses
`VMLAB_HV_VIEWER_WINDOWS` as its disk path. `VMLAB_HV_FIRMWARE_DIR` selects the
directory containing secure OVMF code and Microsoft-enrolled variable defaults.
It checks tablet enumeration and Windows' parsing of the mouse usage and input
report size through SetupAPI and HidP_GetCaps. It sends pointer, button, and key
input and checks that the first Ctrl+Alt press releases the viewer. It does not
observe desktop movement or buttons in Windows' interactive session; those
remain manual checks. No logged-in user, scheduled task, or restart is required.
The investigation and physical-desktop retest steps are in
`../../docs/viewer-windows.md`.

For a real dma-buf import test, select a render node and its matching Vulkan ICD:

```sh
VMLAB_VIEWER_DRM_NODE=/dev/dri/renderD128 \
  cargo test -p vmlab-hv-viewer --lib gpu_dmabuf -- --nocapture
```

Set `VK_DRIVER_FILES` when selecting a different vendor's ICD. This test uses
only render-node allocations, without modesetting or driver changes. Without
`VMLAB_VIEWER_DRM_NODE`, it prints a skip. The test allocates a GBM buffer using
a one-plane modifier advertised by Vulkan, imports it once, and checks bytes
through three GPU write/read and release/reacquire cycles.

The workspace gate is `just ci::check`. To regenerate the committed shaders,
run `glslangValidator -V` on `src/quad.vert` and `src/quad.frag`, with `-o` set
to each source path followed by `.spv`.

## Manual acceptance checklist

Run this checklist on X11 and Wayland, with the NVIDIA GPU and AMD iGPU. Record
the guest, host GPU, compositor, selected frame path, and any CpuCopy diagnostic.

- [ ] A GPU Linux desktop presents virgl and Venus animation without corruption.
- [ ] A Windows desktop presents and accepts typing, pointer, buttons, and wheel.
- [ ] A std-VGA guest remains letterboxed after resize, with aligned pointer input.
- [ ] Clicking grabs. The default and a configured chord both release.
- [ ] Focus loss, close, and disconnect leave no guest keys or buttons held.
- [ ] Num, Caps, and Scroll Lock indicators follow guest state.
- [ ] Guest cursor shape, position, visibility, alpha, and hotspot are correct.
- [ ] Rapid resize produces one final request after 200 ms and the guest applies it.
- [ ] Unicode clipboard text moves both ways while focused, without echo loops.
- [ ] Clipboard polling stops when unfocused and resumes on focus gain.
- [ ] Expose, minimize, restore, and guest mode changes preserve a correct image.
- [ ] Frame pacing stays within one refresh interval during sustained animation.
- [ ] `--shm` uploads partial damage correctly, including alongside a recorder.
- [ ] WSL2 with WSLg negotiates shared memory and passes input/clipboard checks.

## vmlab integration contract

Install `vmlab-hv-viewer` on PATH beside `vmlab-hv`. The later vmlab change must
select it only for vmlab-hv machines and pass `--display`, `--lab-socket`, and
`--machine`. QEMU machines keep their existing viewer. A host-config override
can select the binary and optional release chord. No vmlab application crate is
linked, and this change does not launch or modify the lab daemon.

Keyboard names follow the QEMU QMP QKeyCode reference:
https://www.qemu.org/docs/master/interop/qemu-qmp-ref.html#enum-QKeyCode
Vulkan ownership and import rules follow the Khronos specification:
https://registry.khronos.org/vulkan/specs/latest/html/vkspec.html
