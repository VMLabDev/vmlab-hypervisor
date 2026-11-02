# Control protocol compatibility

This reference covers the JSON-lines control protocol in `vmlab-hv-proto`.
Its source inventory is `src/control.rs`, `src/codec.rs`, `src/client.rs`,
and `src/server.rs` under `crates/vmlab-hv-proto/`.
The separate launch schema is `src/machine.rs`.

## Transport and handshake

Each Unix socket message is one JSON object followed by a newline.
The codec accepts at most 1 MiB per line, excluding the newline.
An oversized line loses framing and requires closing the connection.

The client sends `{"type":"hello","proto_version":1}` first.
The server answers with `type`, `proto_version`, `hv_version`, and
`capabilities`. `PROTO_VERSION` is the protocol major version, currently 1.
Both peers require an exact major match. A server rejects a mismatch with
an id-less `error` carrying `version_mismatch`, then closes the connection.
The client also rejects a mismatched server hello.

There is no minor-version field or minor-version negotiation.
An additive minor release keeps `PROTO_VERSION` unchanged and uses
capabilities to advertise optional commands. A repeated matching hello is
idempotent. Requests before hello receive `no_handshake` with their request
id, and the connection remains open.

## Requests and replies

A request has `type: "request"`, a numeric `id`, and a `command` object
whose `command` field names the operation. A reply has `type: "reply"`,
the same `id`, and `outcome: "ok"` or `outcome: "error"`.
An optional debugger request instead receives `outcome: "debug"` and its
typed `response`. Only peers using the advertised `debug` capability send
those requests. Existing commands retain their existing outcomes.
An error outcome carries string fields `code` and `message`.
An ok outcome may carry `status`. Missing `status` means no run state was
reported, not that the machine has a particular state.

An unknown command decodes to `Command::Unsupported`. After the handshake,
the server returns `unsupported` with message `unsupported request` and the
original request id. It does not call the machine handler or close the socket.
Missing or mistyped required fields of known requests remain malformed.
Unknown outer client message types remain malformed, since they have no
defined request envelope or reply id.

Unknown reply fields are ignored. Unknown error-code strings decode as
`ErrorCode::Unknown(String)`. The outcome remains an error, and consumers
include the original code and message in their generic error report.
Unknown outcomes are malformed, since a consumer cannot classify them as
success or failure.

## Capabilities and events

Unknown capability strings decode as `Capability::Unknown(String)`.
Consumers may log the name but do not enable a feature for it.
The `debug` capability enables the commands described in `docs/debug-feature.md`.
A missing capability is absent, including when the entire `capabilities`
field is missing. Clients only send optional commands when their capability
is advertised. An absent capability never implies support.

Events have `type: "event"`, an `event` name, and no request id.
Unknown event types decode as `Event::Unknown { name }`.
The codec discards their payload and retains their name for logging.
The client returns them from its event APIs, including events buffered while
awaiting a reply. Consumers log and ignore them, then continue reading.
An unknown event does not complete a pending request or change machine state.

Unknown fields of known events are ignored. Optional fields have defaults:
for example, a missing `exiting.error_chain` becomes an empty list.
Malformed required fields of known events remain errors.
Run states, shutdown reasons, and process-exit reasons are closed semantic
vocabularies. Adding values to those existing fields requires a major bump;
a new optional event type can carry new semantics without changing them.

## Changes within a major version

An additive minor release may add capabilities, optional commands,
ignorable event types, error-code strings, and optional reply or event fields.
Existing fields keep their names, types, and meanings. New optional fields
define an absence default in serde. Growing control messages do not use
`deny_unknown_fields`.

A major bump is required to remove or rename an existing operation or field,
change a field type or meaning, require a previously optional field, change
framing or handshake rules, or introduce a mandatory event or outcome.
An event cannot become mandatory merely because the server emits it.

## Strict machine descriptions

`MachineDescription` and its nested configuration objects retain
`deny_unknown_fields`. Unknown hardware fields and unknown hardware enum
values are refused. Ignoring them could launch different hardware from the
configuration the client requested.

Optional launch-schema additions are not automatically safe for older
hypervisors. The client must omit configuration an older server cannot
parse, or require a compatible hypervisor. Control-protocol tolerance does
not relax this launch contract.

## Extra firmware files

The optional `MachineDescription.fw_cfg` field is a list of read-only named
files. Absence means an empty list. This is an additive launch-schema extension
within protocol major 1; older binaries reject the field. Callers must pin a
binary and `vmlab-hv-proto` revision that includes this extension.

Each `FwCfgFile` has a `name` string and a tagged `source` object:

```json
{"fw_cfg":[
  {"name":"opt/vmlab/gop-width","source":{"kind":"inline","bytes":[49,48,50,52]}},
  {"name":"opt/vmlab/gop-height","source":{"kind":"inline","bytes":[55,54,56]}},
  {"name":"bootsplash.jpg","source":{"kind":"file","path":"/run/vmlab/splash.jpg"}},
  {"name":"etc/boot-menu-wait","source":{"kind":"inline","bytes":[0,0]}}
]}
```

`inline.bytes` is a JSON array of u8 values, including zero-length arrays.
`file.path` names a regular host file, read once during machine construction.
Relative paths resolve against the hypervisor's working directory. Sources,
entries, and the machine description reject unknown fields.

Files require firmware boot on q35 or pc. Names contain 1..55 non-NUL ASCII
bytes. Names are case-sensitive and unique. Each payload is at most
4,294,967,295 bytes. Directory selectors occupy `0x0020..0x4000`, excluding
the upper bound and including platform files. Invalid names, duplicates,
reserved names, oversized payloads, unreadable paths, and a full directory
fail machine construction with a named error. These are launch failures,
not control-socket error replies.

Platform-owned names are reserved even if the selected machine omits them:
`bootorder`, `bios-geometry`, `etc/e820`, `etc/boot-fail-wait`,
`etc/system-states`, `etc/table-loader`, `etc/acpi/rsdp`, `etc/acpi/tables`,
`etc/smbios/smbios-anchor`, `etc/smbios/smbios-tables`, `etc/tpm/config`,
`etc/tpm/log`, `etc/smi/supported-features`, `etc/smi/requested-features`,
and `etc/smi/features-ok`.

Both port reads and DMA reads serve the captured bytes; guest writes fail.
Reset retains the captured inputs without reopening host paths. Snapshot
restore requires the same file names and read-only contents. The machine
digest also covers the launch description. Changing a host file after launch
does not change the running VM; a new launch with different bytes rejects
the old fw_cfg snapshot state.

For vmlab's GOP-capable OVMF, width and height are decimal ASCII, with no
required NUL. The resolver must check the firmware's `gop_resolution`
capability and provide both dimensions, with width 320..8192 and height
200..8192. The generic file transport does not interpret firmware-specific
payloads. SeaBIOS consumes JPEG bytes from `bootsplash.jpg` and a
little-endian u16 millisecond delay from `etc/boot-menu-wait`; zero is the
branded firmware's POST-only splash setting.

`Boot::Firmware.boot_menu` is an optional boolean, default false. Setting it
to true enables the numeric fw_cfg boot-menu flag. SeaBIOS requires this flag
to wait for `etc/boot-menu-wait`; the S1 test enables it with a 10,000 ms delay.
The branded firmware's zero-delay POST splash keeps `boot_menu` false,
matching vmlab's QEMU `menu=off,splash-time=0` launch.

Sources: `crates/vmlab-hv-proto/src/machine.rs`,
`crates/vmlab-hv-vmm/src/builder.rs`,
`crates/vmlab-hv-firmware/src/fw_cfg.rs`, and vmlab draft PR #205.
The guest directory and DMA formats follow
https://www.qemu.org/docs/master/specs/fw_cfg.html.

`crates/vmlab-hv/tests/fw_cfg.rs` exercises firmware consumption through S1
and captures native-display frames before and after snapshot restore.
`VMLAB_HV_TEST_FIRMWARE_DIR` selects the vmlab firmware bundle with
`features.json`, `OVMF_CODE_4M.fd`, `OVMF_VARS_4M.fd`, and
`bios-vmlab-q35.bin`. The test uses a bundled two-colour JPEG fixture.
`VMLAB_HV_TEST_FW_EVIDENCE_DIR` optionally retains PPM screenshots.

## Compatibility verification

`tests/compatibility.rs` covers future hello values and events, missing
optional fields, unknown error codes, unsupported requests, and malformed
known messages. `tests/wire.rs` pins existing wire shapes and strict machine
fields. The S1 control compatibility test drives the real binary's socket.

These rules apply to peers built with tolerant decoding. Previously released
clients with closed enums still need an update before they can accept new
capabilities or event types. A server update cannot repair an old decoder.
