# Viewer cursor and Windows input fixes

## Verification outcome

The workspace gate is not green. Its Windows 7 acceptance test failed during
concurrent validation, and the owner stopped the already-failed gate while a
later test waited for capacity under its original four-VM environment. A device
test continuation passed 437 tests but timed out in the unchanged virtio-net
link reconnect test. The other workspace packages then passed with
`--no-fail-fast`. The full suite was not rerun.

At the owner's request, the two failures were run individually, in order:

- V1. `pc_acceptance::windows_7_agent_snapshot_reset` passed alone in 157.97 s.
  Snapshot rollback completed in 7.06 s, and reset retained the process,
  control connection, and agent socket. Evidence is in
  `.scratch/viewer-windows/isolated-win7.log`.
- V2. `virtio::net::tests::link_drops_and_reconnects_with_frames_discarded_while_down`
  passed 20 consecutive isolated runs. Evidence is in
  `.scratch/viewer-windows/isolated-net-20.log`.

These are recorded as load-sensitive failures, not fixed by this work. The
initial batch failures remain in `.scratch/viewer-windows/ci.log` and
`ci-remaining.log`. Formatting and workspace Clippy passed. Focused viewer,
USB, private-display, Linux S1, and Windows S1 checks cover the changes below.

## Causes

F1. Grabbing hid the host cursor even when the guest supplied no cursor shape.
Headless probes of the owner's Windows 11, Ubuntu console, and SteamOS display
sockets all initially received an empty shape and a hidden CursorState.
Ubuntu was a text console, so it supplied no evidence that desktop pointer input
worked. SteamOS's initial cursor state alone does not establish its desktop state.
The disappearance can affect any guest without a separate cursor shape.

The first viewer fix retained the host cursor for an empty guest shape. After
restarting on the corrected hypervisor, the owner confirmed mouse delivery but
saw both the host cursor and Windows' software cursor. Windows still sends an
empty hardware cursor shape; its cursor pixels are in the framebuffer.

The owner approved hiding the host cursor over active guest frames, without
adding an extra local arrow to those frames. Hardware cursor messages remain
composited with their hotspot and visibility. A local fallback arrow is used
only for host placeholder frames without a guest shape. It follows pointer
motion without keyboard capture. Leaving the window or having no frame restores
the host cursor. Absolute pointer, button, and wheel input do not need capture.

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
The shell releases native keyboard capture before sending guest key releases.
It no longer requests pointer confinement or locking. A no-capture chord stays
uncaptured; clicking with the complete release chord held cannot capture input.
Only a left-click can request keyboard capture. The owner's exact KDE
confinement sequence was not reproduced on the private display, but the old
held-chord capture and suppressed ungrabbed pointer input were reproduced.

## Regression checks

- T1. Fake display-server tests cover cursor fallback, hotspot preservation,
  move-only and hide updates, both chord orders, pre-grab modifiers, repeated
  grabs, interleaved LEDs, focus loss, matching guest key releases, no-capture
  chords, held-chord clicks, and buttons without keyboard capture.
- T2. Private Xvfb tests inspect displayed pixels and the X cursor image. They
  cover ungrabbed absolute motion, host cursor entry/leave policy, software
  frames without a fallback overlay, hardware cursor takeover and hotspots,
  move-only updates, placeholder fallback motion, no-capture chords, held-chord
  clicks, free pointer exit while keyboard capture is active, and native keyboard
  release on the first chord. Both mailbox and
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
  The owner confirmed Windows desktop mouse input after restarting on the
  corrected hypervisor. Firmware, disk, and test
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
Subsequent focused runs use `VMLAB_HV_TEST_VM_LIMIT=6`. The original gate kept
its four-VM environment and was stopped at the owner's request after a failure.
Private hypervisor binaries must retain
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
2. Move across the window without clicking. Check corners, buttons, wheel,
   hidden host cursor inside, and restored host cursor outside. Click to capture
   the keyboard, then move out freely. Repeat on SteamOS in KDE Plasma.
3. Press left Ctrl then left Alt once. Repeat with the opposite order, and with
   one modifier held before clicking to grab. Each attempt must release the
   keyboard grab immediately and leave no guest modifier or mouse button held.
   Press the chord before capturing and click while holding it. Neither action
   may start capture or confine the pointer.
4. Where the guest supplies a hardware cursor, check its hotspot, move-only
   updates, shape changes, and intentional hiding. Record whether the viewer
   used a hardware cursor, framebuffer software cursor, or placeholder arrow.

## Display reconnect and clipboard follow-up

F4. The viewer treated every display receive error as fatal. The previous
executable reproduced the owner's `Error: unexpected end of file` in private
Xvfb when its display peer closed. The new viewer keeps the same window, clears
its image, and shows `waiting for display` in the title. It releases host
keyboard capture, restores the host cursor, and reconnects off the UI thread.
Retries grow from 100 ms to 2 s. A pidfd tracks the original socket peer, so
removing or rebinding the socket does not mean the machine has exited.
The viewer exits when that process dies or the user closes the window.
Initial connection and local renderer setup errors still fail startup.

Reconnect resets surface IDs, buffer imports, repaint images, cursor state,
LEDs, and resize state. Previously held guest keys and buttons get matching
releases on the new connection. Input while disconnected is not replayed.
Connection generations prevent queued old packets from modifying new state.
Handshake attempts have a two-second timeout and cancellation closes the
active stream before joining the reader.

F5. In-place reset does not deliberately disconnect display clients. Linux S1
reboot retained the process, control connection, and original display
connection in 28.11 s. A separate deterministic server regression reproduced
a GPU reset race: a readback from an obsolete frame generation failed after
reset, and the server closed the client. The server now drops that failed
obsolete frame and continues with the new generation. A failure from the
current generation still propagates.

The owner's original disconnect cannot be attributed conclusively. The old
server discarded native-client errors. Its log contains GPU context/import
warnings and tablet queue warnings, but no client-close reason. The reproduced
readback race is a verified server failure path, not proof of the owner's
specific sequence. Native input errors now reach the caller, and the VMM logs
client errors. No native display message or shared device trait changed.

F6. Clipboard failures had no backoff, so different lab error strings could
alternate every second. The bridge now reports one outage, retries with
exponential delays from 1 s to 30 s, and reports recovery once. Refocusing does
not reset the delay. Busy host clipboards, EAGAIN, interrupted operations, and
timeouts retry without an error line. Failed transfers retain pending state.
Lab replies have an absolute deadline as well as a per-read timeout.

The old bridge ignored host-read failures. The unprefixed
`Resource temporarily unavailable (os error 11)` could therefore also come
from the lab socket's receive timeout, rather than a nonblocking host read.
Both paths now retry. Arboard's Wayland adapter erases the I/O error type;
the bridge recognizes its EAGAIN description as a temporary error too.

T5. Private Xvfb checks run both presentation modes and verify the same native
window across EOF, socket removal, rebind, and reused surface IDs. They inspect
waiting and resumed pixels, host cursor restoration, native keyboard release,
and guest key/button releases after reconnect. The old executable fails this
reproduction. Reader tests cover truncated packets, stalled-handshake
cancellation, and original-process death.

T6. Clipboard tests use real fake-lab sockets to verify alternating errors,
delay growth and its cap, focus changes during backoff, socket timeout retry,
and a single recovery notice. Existing clipboard synchronization tests still
cover host changes, guest polling, and echo suppression.

Evidence is in `.scratch/viewer-windows/reconnect-before.log`,
`reconnect-after.log`, `server-reset-before.log`, `server-reset-after.log`,
`reset-s1.log`, and `reconnect-clippy.log`. Focused tests, formatting, and
workspace Clippy pass. The full workspace suite was not rerun, as requested.
The optional hardware dma-buf test still skips without a selected render node.
The owner's Windows Venus first-boot reboot and physical KDE clipboard retest
remain manual checks. No owner VM or desktop was changed.
