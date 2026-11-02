# Viewer cursor and Windows input fixes

## Causes

F1. Grabbing hid the host cursor even when the guest supplied no cursor shape.
Headless probes of the owner's Windows 11, Ubuntu console, and SteamOS display
sockets all initially received an empty shape and a hidden CursorState.
Ubuntu was a text console, so it supplied no evidence that desktop pointer input
worked. SteamOS's initial cursor state alone does not establish its desktop state.
The disappearance can affect any guest without a separate cursor shape.

The viewer now retains the host cursor when the guest supplies no shape. A locked
pointer uses a local arrow at the translated absolute position. A nonempty guest
shape takes over, including the guest's explicit visibility setting. The renderer
already applies hotspots and redraws move-only cursor updates; those paths did
not need a rendering change.

F2. Windows could not enumerate the tablet. Its USB root hub reported Code 43 and
"The USB hub failed to reset." Ubuntu enumerated the same emulated tablet.
A disposable Windows 11 standard overlay reproduced the failure. The controller
trace showed repeated GET_DESCRIPTOR requests rejected with TRB Error, code 5.
Windows submitted Setup+Data+Event Data, then waited for completion before
submitting Status. The controller incorrectly required Status in the first
batch. It now completes the data event and accepts the subsequent Status stage
without repeating the atomic USB request. No coordinate scaling or HID report
descriptor change was needed.

The controller change follows the control-stage and Event Data rules in xHCI
sections 3.2.9 and 4.11.2.2:
https://www.intel.com/content/dam/www/public/us/en/documents/technical-specifications/extensible-host-controler-interface-usb-xhci.pdf

F3. Release-chord tracking discarded keys pressed before grab. It also sent a
key-up for the completing chord key even though that key's press stayed local.
Both defects reproduce without a guest. Physical key state is now separate from
forwarded guest key state. Either key order releases on the completing press,
including a modifier held before clicking or across re-grab. Focus loss clears
physical state and releases forwarded input. LED messages do not affect it.
The shell releases the native grabs before sending key releases to the guest.
The owner's exact repeated-press sequence was not directly observed.

## Regression checks

- T1. Fake display-server tests cover cursor fallback, hotspot preservation,
  move-only and hide updates, both chord orders, pre-grab modifiers, repeated
  grabs, interleaved LEDs, focus loss, and matching guest key releases.
- T2. Private Xvfb tests inspect displayed pixels and the X cursor image. They
  cover no-shape grabs, guest cursor takeover, hotspot alignment, move-only
  updates without new frames, explicit hiding, empty-shape fallback, and
  first-press native release with a modifier held before grab. Both mailbox and
  FIFO presentation modes run. These tests never use the owner's display.
- T3. The USB device regression uses the captured Windows transfer layout.
  Descriptor data must complete successfully before Status is queued, including
  a snapshot and restore between the stages. All USB tests run without claiming
  physical USB devices.
- T4. Windows S1 boots a disposable standard-template overlay through the
  hypervisor control socket. It checks tablet enumeration and Windows' parsing
  of mouse usage and input report size through SetupAPI and HidP_GetCaps. It
  sends pointer, button, and key input and exercises the first Ctrl+Alt release.
  It does not assert desktop movement or button state in an interactive Windows
  session. Linux S1 and the USB device tests observe coordinates and reports;
  Windows desktop input remains a manual check. Firmware, disk, and test
  selection are documented in the viewer README.

Run focused checks with:

```sh
cargo test -p vmlab-hv-viewer -- --nocapture
cargo test -p vmlab-hv-devices usb:: -- --nocapture
```

VM tests use the host-wide permit and a systemd scope capped at 24 GiB with no
swap. The owner's three lab VMs stay running. Temporary overlays and sockets are
removed when the test exits. No display-protocol fields, shared device traits,
or snapshot formats changed.

The owner raised this run's host-wide limit from four to six.
Subsequent focused runs use `VMLAB_HV_TEST_VM_LIMIT=6`; the gate already running
keeps its original four-VM environment. Private hypervisor binaries must retain
the `vmlab-hv-test-` prefix so the shared process counter recognizes them.

## Owner desktop retest

Physical X11 and Wayland input still need an owner retest after rebuilding.
The private Xvfb checks cannot establish compositor-specific shortcut inhibition
or physical input timing. No owner VM was stopped or restarted during this work.

Windows 11 test setup did not restore the agent connection within 120 seconds
after either an in-process reboot or a clean shutdown and cold-start following
automatic-login configuration. That remains unresolved and is outside this
input fix. The final HID test needs neither login nor restart; its success does
not establish restart correctness. The opt-in hardware
dma-buf import test was not run because no render node was selected.
An attempted HidD_GetInputReport probe returned "Incorrect function" from
Windows. HID capability queries are not a substitute for observing desktop
input, so the manual pointer and button checks below remain required.

1. On Windows 11, check that USB Root Hub and HID-compliant mouse both report OK
   after starting a guest with the rebuilt hypervisor.
2. Click the viewer and move across the window. Check corners, button clicks,
   and cursor visibility. Repeat on SteamOS after switching to KDE Plasma.
3. Press left Ctrl then left Alt once. Repeat with the opposite order, and with
   one modifier held before clicking to grab. Each attempt must release the
   native grab immediately and leave no guest modifier or mouse button held.
4. Where the guest supplies a hardware cursor, check its hotspot, move-only
   updates, shape changes, and intentional hiding. Record whether the viewer
   used a guest shape, host cursor, or locked-pointer local arrow.
