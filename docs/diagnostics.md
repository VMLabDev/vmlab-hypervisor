# Crash and exit diagnostics

`vmlab-hv` writes JSON log lines to stderr. vmlab captures stderr in the run
directory's `vmlab-hv.log`. Diagnostics do not use the guest's serial log.

## Log settings

`VMLAB_HV_LOG` selects the stderr filter at startup. Its default is `info`.
The syntax is the `log` ecosystem's `env_filter` syntax: comma-separated
`target=level` directives, an optional global level, and an optional message
regular expression after `/`. Levels are `off`, `error`, `warn`, `info`,
`debug`, and `trace`. Targets are Rust module paths with underscores.

For example, `VMLAB_HV_LOG=info,vmlab_hv_devices=debug` includes device debug
messages. Invalid startup filters fail with exit code 2.

Normal log records contain `unix_ns`, `level`, `thread`, `vcpu`, `target`, and
`message`. `vcpu` is populated for threads named `vcpu-N`. The logger retains
the most recent 1,024 records at all levels, even when stderr is `off`.
Messages are formatted once for the recorder, redacted, and truncated to
4,096 UTF-8 bytes. Stderr filtering adds no separate message-formatting pass.

`VMLAB_HV_DIAGNOSTICS`, when present, writes a report on normal exit too.
It previously selected an ad-hoc timer dump. Timer information now appears in
every report under `vm.timers`.

### Exact per-exit RIP

The per-vCPU ring always retains the last 256 exits: reason, port or MMIO
address, size, direction, timestamp, and ioctl error when present. Its default
mode has `rip: null`. It requests no `KVM_SYNC_X86_REGS` copy and performs no
`KVM_GET_REGS` ioctl in the exit loop.

Exact per-exit RIP is opt-in. This startup filter enables it:

```sh
VMLAB_HV_LOG=info,vmlab_hv_vmm::diagnostics::flight=debug
```

Debug or trace on the recorder target enables RIP capture. A global `debug`
or `trace`, or a matching parent target such as `vmlab_hv_vmm=debug`, also
enables it. `info` or lower on the recorder target disables it. For example,
`VMLAB_HV_LOG=debug,vmlab_hv_vmm::diagnostics::flight=info` retains other debug
logs without per-exit register copying. Debug on an unrelated device target
does not enable RIP capture. Message regexes do not affect this target/level
selection, and the all-level log ring does not enable it.

The `set_log_filter` control request switches the same mode on and off at
runtime, as shown below. vCPUs are kicked and apply the selection before their
next `KVM_RUN`. Existing history is preserved: old RIP samples remain until
the ring overwrites them, and enabling capture does not fill earlier entries.
Unsupported kernels retain `rip: null` even when capture is requested.

Reports still attempt to read every vCPU's general and special registers,
independently of this switch. Fatal-signal reports retain the checkpoint
freshness limits described below.

## Exit outcomes

The final `exiting` control event identifies the reason separately from the
process status. Guest poweroff, reset with `exit_on_reset`, and `quit` share
code 0 because the project specification defines all three as clean shutdowns.

| Reason | Exit code | Automatic report |
| --- | ---: | --- |
| `guest_poweroff` | 0 | With `VMLAB_HV_DIAGNOSTICS` |
| `guest_reset` with `exit_on_reset` | 0 | With `VMLAB_HV_DIAGNOSTICS` |
| `quit` | 0 | With `VMLAB_HV_DIAGNOSTICS` |
| `runtime_error` | 1 | Yes |
| `startup_error` | 2 | For construction errors, with `VMLAB_HV_DIAGNOSTICS` |
| `panic` in any Rust thread | 3 | Yes |
| `guest_panic` shutdown reason | 4 | Yes |
| `host_signal` for SIGINT, SIGQUIT, SIGTERM | 130, 131, 143 | With `VMLAB_HV_DIAGNOSTICS` |
| `fatal_signal` for SIGILL, SIGABRT, SIGBUS, SIGFPE, SIGSEGV | 132, 134, 135, 136, 139 | Yes, checkpoint plus fault record |

Fatal signals terminate through the signal's default action. A Unix wait
status therefore reports a signal, rather than `exit(128 + signal)`.
The numbers in the table are shell conventions and the report's code mapping.
SIGKILL, host power loss, and kernel OOM kills cannot run a userspace handler.

A device cleanup failure changes an otherwise clean outcome to `runtime_error`.
Cleanup errors append to the original runtime error chain. The chain retains
`Error::source()` causes when available; existing string-only device errors
remain a single cause. A Rust panic writes a backtrace and exits immediately,
without unwinding through potentially damaged device state.

## Runtime requests

The version-1 handshake advertises the additive `diagnostics` capability.
Requests require the normal version handshake. These JSON lines are examples
for an established control connection:

```json
{"type":"request","id":10,"command":{"command":"set_log_filter","filter":"info,vmlab_hv_devices=debug"}}
{"type":"request","id":11,"command":{"command":"dump_diagnostics"}}
```

`set_log_filter` replaces the stderr filter and its exact-RIP selection.
Both rings remain enabled. Success returns the normal `ok` reply with no
status. An invalid filter returns `invalid_argument` and preserves both
the previous filter and RIP selection.

These requests enable exact RIP, then disable it while retaining the default
info logs. They use the same version-1 handshake and `diagnostics` capability;
no additional command or capability is required:

```json
{"type":"request","id":12,"command":{"command":"set_log_filter","filter":"info,vmlab_hv_vmm::diagnostics::flight=debug"}}
{"type":"request","id":13,"command":{"command":"set_log_filter","filter":"info"}}
```

`dump_diagnostics` writes a report beside the control socket. Success emits
`diagnostics_dumped {path}` and returns an `ok` reply with no status. A write
failure returns `internal`. The dump does not pause the machine, so busy state
can be absent. On-demand reports have `capture: "on_demand"`, `reason: null`,
and `exit_code: null`. They do not predict how the process will end.

After workers and devices finish, the server broadcasts
`exiting {reason, code, error_chain}` before closing the connections. The older
`shutdown` event remains unchanged and precedes this final event. A connected
client also receives the reply to an in-flight `quit`. A panic attempts the
final event while the socket is available. Fatal handlers do not use sockets.
Clients must recognize the new event variants when using the updated protocol.

## Read a report

Reports are named `crash-<unix-ms>.json` beside the control socket. Files use
mode 0600 and creation never replaces an existing report. The final stderr line
has level `ERROR` and names the report, even when `VMLAB_HV_LOG=off`. If writing
fails, that line describes the failure instead.

Start with `reason`, `exit_code`, `error_chain`, and `panic`. Then inspect
`logs` and `vm.vcpus[].exits`, which are ordered oldest to newest.

The schema-version-1 report contains these fields:

| Fields | Meaning |
| --- | --- |
| `hv_version`, `git_revision`, `kernel`, `pid`, `unix_ms` | Build and host identity, capture time |
| `machine` | Machine description with private fields redacted |
| `reason`, `exit_code`, `error_chain` | Process outcome and causes |
| `panic.message`, `panic.backtrace` | Panic details, independent of `RUST_BACKTRACE` |
| `logs` | Bounded, redacted records, including debug and trace |
| `vm.vcpus[].registers` | General registers, special registers, MP state, and `sync_valid_regs` KVM mask |
| `vm.vcpus[].exits` | Last 256 KVM_RUN outcomes for that vCPU |
| `vm.exit_rip.requested`, `vm.exit_rip.supported` | Recorder debug selection and kernel sync-register support |
| `vm.timers` | Hyper-V MSRs and SIMP timer headers, LAPIC, PIT, PIC, IOAPIC, HPET |
| `vm.pci_bars`, `vm.mch_smram`, `vm.usb_attachments` | Cheap device summaries |
| `process_status`, `thread_count`, `threads` | Linux memory, scheduling, and thread statistics |

Exit records contain a Unix timestamp in nanoseconds, numeric `KVM_EXIT_*`
reason, `rip`, `address`, `size`, `write`, and `errno`. `address` is a port or
MMIO address; `size` is the access width. No I/O payload is retained.
An interrupted or failed ioctl uses reason 4294967295 and carries `errno`.
Per-exit `rip` is null by default. When exact RIP is enabled on a supporting
kernel, `KVM_SYNC_X86_REGS` copies general registers through the shared KVM
mapping, without an extra ioctl per exit. Older kernels retain null RIP.
The `regs` and `sregs` report fields are read independently of exit recording.

Busy locks and unreadable KVM registers produce null fields or explicit busy
entries. Reports never wait for a device lock held by a panicking thread.
They contain no guest RAM dump, guest screen text, or device snapshot blobs.
General and special registers use named fields, including `rip`, `cr0`, and
segment descriptors, rather than KVM's snapshot ABI byte arrays.

Machine fields are public only after their paths appear in the diagnostic
allowlist. New fields default to `[REDACTED]`. Secret-key names, including OSK,
password, credential, token, and private-key fields, are also redacted.
String values from private fields are scrubbed from messages, errors, and
backtraces before output. Synthetic test values are generated at runtime;
no actual OSK is a fixture. Host paths and supported public configuration
remain visible, so reports still need appropriate access controls.

### Fatal reports

A background worker prepares a checkpoint approximately once per second,
with another checkpoint during startup. Two pre-opened staging files prevent
the handler from reading a partially written checkpoint. Capture can take
longer under load; `unix_ms` is the checkpoint time, not the fault time.

The SIGSEGV, SIGBUS, SIGILL, SIGFPE, and SIGABRT handlers run on an alternate
signal stack. They append only a bounded fault record to the published
checkpoint, publish its filename, write the prepared stderr path line, and
re-raise with the default disposition. The handler allocates no memory,
formats no Rust strings, and takes no mutexes. Rust threads retain their
runtime's alternate stacks. Explicit entry, vCPU, and control-thread registration
supplies a 64 KiB stack when the calling thread has none.

For a fatal report, `reason.kind` is `fatal_signal`, and `fault` carries the
exact `signal`, `exit_code`, `address`, Linux `tid`, and `unix_ns`. The top-level
`exit_code` is null. Address is zero for explicitly raised signals without a
hardware fault address. Logs, exit rings, registers, and statistics come from
the last checkpoint and can predate the fault. Core files provide the final
native thread stacks. Partial startup, simultaneous fatal faults, damaged
memory, and storage failures can limit reporting.

Staging files have names `.crash-<ms>-<pid>-<slot>.pending`. Clean exits remove
them. A fatal signal or SIGKILL can leave an unused staging file. If the final
name already exists, the handler preserves it and names the completed staging
file on stderr instead.

## Enable host core dumps

Core generation depends on the host's `RLIMIT_CORE` and `kernel.core_pattern`.
For a shell-launched process, `ulimit -c unlimited` permits a core. A systemd
service needs an appropriate `LimitCORE=` setting. On hosts with
systemd-coredump, `coredumpctl info` and `coredumpctl debug` locate and inspect
the native core. Fatal-signal tests disable core files in their child process.

Guest RAM, A20 CPU aliases, PAM shadow RAM, and SMRAM carry `MADV_DONTDUMP`,
including aliases recreated with `MAP_FIXED`. Thus a host core excludes bulk
guest memory. Host heap data can still contain credentials; core files do not
receive JSON-report redaction.

Linux's signal and core contracts are documented at
https://man7.org/linux/man-pages/man2/sigaction.2.html,
https://man7.org/linux/man-pages/man7/signal-safety.7.html, and
https://man7.org/linux/man-pages/man2/madvise.2.html.

## Verification and recorder cost

The source inventory is `vmlab-hv-proto/src/control.rs`,
`vmlab-hv/src/main.rs`, `vmlab-hv-vmm/src/vcpu.rs`, and
`vmlab-hv-vmm/src/diagnostics/` under `crates/`.
Unit tests cover outcome codes, bounds, redaction, panic reports, real SIGSEGV
delivery in a child, and RAM's Linux `dd` mapping flag. The S1 tests in
`crates/vmlab-hv/tests/diagnostics.rs` exercise the binary's control socket.
Their firmware is generated locally and uses no host passthrough device.
Each VM uses a host-wide harness slot and a 24 GiB, no-swap systemd scope.

`VMLAB_HV_TEST_RUNTIME_ERROR=1`, with the binary's `testing` feature, deliberately
fails `resume`. It is absent from production builds. The S1 runtime-error test
builds that feature explicitly and checks code 1, the final event, and report.

The userspace vCPU-exit hook microbenchmark runs without a VM:

```sh
CARGO_BUILD_JOBS=4 taskset -c 0-7 cargo test --release -p vmlab-hv-vmm \
  vcpu::tests::recorder_exit_hook_overhead -- --ignored --nocapture
```

On the development host, 2,000,000 iterations measured 0.32 ns per baseline
iteration and 30.52 ns with exit decoding, timestamp, and ring insertion,
a 30.20 ns delta. The benchmark calls the default-mode hook with synthetic KVM
metadata. It excludes KVM register-copy cost, checkpoint contention, and guest
throughput; it is not a whole-VM performance claim.

The real KVM loop benchmark compares recording disabled, default metadata,
opt-in exact RIP, and a timestamp-only control without ring insertion. It
includes the cached-mode check in both recording modes and kernel
register-copy cost only in the opt-in mode. It requires
`/dev/kvm`, sync-register support, and a memory-capped scope. It creates a KVM
VM inside the test binary, without a `vmlab-hv` or `qemu-system` process.
The host's test-window policy must permit in-process KVM tests. Its command is:

```sh
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  env CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=1 taskset -c 14 \
  cargo test --release -p vmlab-hv-vmm \
  vcpu::tests::recorder_kvm_loop_overhead -- --ignored --nocapture
```

Three repetitions on CPU 14, outside CI's CPU 0-7 affinity, each discarded a
warm-up quartet and used nine alternating quartets of 200,000 exits per sample.
The guest repeatedly wrote one byte to port 80h. The default-mode samples
asserted that `KVM_SYNC_X86_REGS` was clear and every recorded RIP was null.
The opt-in samples asserted the opposite. Results were:

| Repetition | Mode | Baseline ns/exit | Recorded ns/exit | Delta ns/exit | Overhead |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | Default metadata | 3358.56 | 3506.74 | 148.19 | 4.41% |
| 1 | Opt-in exact RIP | 3358.56 | 3549.22 | 190.67 | 5.68% |
| 1 | Timestamp only | 3358.56 | 3490.07 | 131.51 | 3.92% |
| 2 | Default metadata | 3389.74 | 3532.38 | 142.63 | 4.21% |
| 2 | Opt-in exact RIP | 3389.74 | 3582.99 | 193.24 | 5.70% |
| 2 | Timestamp only | 3389.74 | 3499.56 | 109.82 | 3.24% |
| 3 | Default metadata | 3376.00 | 3515.84 | 139.84 | 4.14% |
| 3 | Opt-in exact RIP | 3376.00 | 3574.03 | 198.03 | 5.87% |
| 3 | Timestamp only | 3376.00 | 3496.17 | 120.17 | 3.56% |

The default no longer copies general registers, but its measured overhead
is not near zero in this exit-saturated workload. The timestamp-only control
also has measurable cost, without register copying or a ring. The
userspace-only microbenchmark excludes the KVM transition context and must
not stand in for the full-loop result. Representative guest throughput has
not been measured.

The seven S1 diagnostics tests cover guest poweroff, guest reset,
SIGTERM, runtime failure, filter changes, on-demand reports, quit, and
startup-error redaction. They also check default metadata, startup debug,
runtime RIP capture in both directions, unchanged selection after an invalid
filter, and crash-time register reads for both vCPUs with per-exit RIP off.

### Illustrative report excerpt

This default-mode excerpt omits large arrays and is not a complete captured report:

```json
{
  "schema_version": 1,
  "hv_version": "0.1.0",
  "reason": {"kind": "runtime_error"},
  "exit_code": 1,
  "error_chain": ["vcpu 0: KVM_RUN", "Input/output error"],
  "panic": null,
  "machine": {"name": "example", "applesmc": "[REDACTED]"},
  "logs": [{"level": "DEBUG", "thread": "vcpu-0", "vcpu": 0,
            "unix_ns": 1793491200000000000, "target": "vmlab_hv_vmm",
            "message": "device dispatch completed"}],
  "vm": {"vcpus": [{"id": 0, "exits": [
    {"unix_ns": 1793491200000000000, "reason": 2, "rip": null,
     "address": 1016, "size": 1, "write": true, "errno": null}
  ]}]}
}
```
