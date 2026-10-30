# Display integration acceptance

The P3 test `ubuntu_26_04_native_display_input_and_rfb` runs the normal
`vmlab-hv` binary through its control, native display, RFB and agent sockets.
Its implementation is in `crates/vmlab-hv/src/display_acceptance.rs` and
`crates/vmlab-hv/tests/{guest_display,rfb_client}.py`.

## Prerequisites and command

The test requires KVM, SeaBIOS, SeaVGABIOS for virtio-vga, the local Ubuntu
26.04 template, Python 3 and Tesseract with English recognition data. Ubuntu
must have Python 3 and the virtio-gpu and input drivers. The shipped template
has these. Each run creates a disposable qcow2 overlay and never writes the
template.

```sh
cargo build -p vmlab-hv
cargo test -p vmlab-hv ubuntu_26_04_native_display_input_and_rfb -- --nocapture
```

Without KVM, firmware, the template or Tesseract, the test prints a named skip.
Other prerequisite or guest failures fail the test. `just ci::check` includes
this test. Build the binary first when exercising a changed executable.

## Observations

- A native client connects to an explicitly configured socket before resume
  and receives the initial paused surface. Default socket paths remain absent.
- Native memfd pixels contain the SeaBIOS banner and later text written to the
  Linux console. Tesseract recognizes both; a nonblank frame alone cannot pass.
- The guest reports `virtio_gpudrmfb`. A preferred-size request followed by
  guest DRM modesetting produces an 800x600 surface with full damage.
- The guest installs a white 64x64 hardware cursor. Native messages preserve
  its pixels, hotspot, position and visibility.
- Replacing the guest scanout with another framebuffer at the same resolution
  produces a fresh surface configure and different pixels.
- Guest `/dev/input/event*` records verify native and RFB keys, absolute pointer
  positions, left and middle buttons, and downward wheel direction. Caps Lock
  updates the native LED state.
- An independent Python RFB 3.8 client negotiates Raw encoding, receives a full
  nonblank frame, and sends keys and pointer events.

The guest probe uses Linux input and DRM UAPI. It does not bypass the emulated
devices or insert host-side fake frames. The vmlab integration owner runs the
separate vision and recorder S4 checks.

## Overrides and artifacts

| Variable | Meaning |
|---|---|
| `VMLAB_HV_TEMPLATE_UBUNTU_26_04` | Template disk path; otherwise use the latest local version |
| `VMLAB_HV_BINARY` | Executable path; otherwise use `target/debug/vmlab-hv` |
| `VMLAB_HV_TEMPLATE_TIMEOUT` | Agent-ready timeout in seconds; default 120 |
| `VMLAB_HV_DISPLAY_ARTIFACTS` | Existing directory for OCR input images; absent writes no persistent images |

The display wire format, socket-path defaults and input semantics are in
`crates/vmlab-hv-display/PROTOCOL.md`.
