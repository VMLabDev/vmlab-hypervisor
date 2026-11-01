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

## Verification and rollout

`tests/compatibility.rs` covers future hello values and events, missing
optional fields, unknown error codes, unsupported requests, and malformed
known messages. `tests/wire.rs` pins existing wire shapes and strict machine
fields. The S1 control compatibility test drives the real binary's socket.

These rules apply to peers built with tolerant decoding. Previously released
clients with closed enums still need an update before they can accept new
capabilities or event types. A server update cannot repair an old decoder.
