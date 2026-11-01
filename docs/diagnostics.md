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

### Per-exit flight recorder

The per-vCPU flight recorder is off by default. The exit loop reads a cached
mode and skips recording: no ring allocation, timestamp read, metadata decoding,
history stores, `KVM_SYNC_X86_REGS` copy, or `KVM_GET_REGS` ioctl. Structured
logging and its all-level log ring, exit outcomes, panic and fatal-signal hooks,
`MADV_DONTDUMP`, and crash reports remain enabled independently.

This startup filter enables the last 256 exits with exact per-exit RIP:

```sh
VMLAB_HV_LOG=info,vmlab_hv_vmm::diagnostics::flight=debug
```

This filter enables metadata recording without per-exit register copying:

```sh
VMLAB_HV_LOG=info,vmlab_hv_vmm::diagnostics::flight::metadata=debug
```

Both modes retain reason, port or MMIO address, size, direction, timestamp,
and ioctl error when present. Metadata-only entries have `rip: null`.
Debug or trace on the base recorder target enables RIP capture. A global `debug`
or `trace`, or a matching parent target such as `vmlab_hv_vmm=debug`, also
enables it. `info` or lower disables the recorder unless the metadata child
target enables it. For example,
`VMLAB_HV_LOG=debug,vmlab_hv_vmm::diagnostics::flight=info` retains other debug
logs without per-exit recording. Debug on an unrelated device target
does not enable recording. Message regexes do not affect this target/level
selection, and the all-level log ring does not enable it.

The `set_log_filter` control request switches the same mode on and off at
runtime, as shown below. vCPUs are kicked and apply the selection before their
next `KVM_RUN`. Existing history is preserved and stops changing when recording
is disabled. Enabling capture does not fill earlier entries, and changing
between metadata and RIP modes leaves older entries until overwritten.
Unsupported kernels retain `rip: null` even when capture is requested.

Each ring has one vCPU writer. Records use atomic stores, without locks,
atomic read-modify-write operations, allocation, or formatting. Entries and
publication state occupy separate 64-byte cache lines. Dump readers copy
complete generations without blocking the writer; an overwritten or unfinished
copy returns null history rather than waiting.

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

`set_log_filter` replaces the stderr filter and its flight-recorder selection.
The log ring remains enabled. Success returns the normal `ok` reply with no
status. An invalid filter returns `invalid_argument` and preserves the previous
filter and recorder mode.

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
| `vm.vcpus[].exits` | Last 256 recorded KVM_RUN outcomes, null if never enabled or a copy was busy |
| `vm.vcpus[].exit_clock` | Timestamp source and calibrated TSC frequency, null if never enabled |
| `vm.exit_recorder.enabled` | Whether new exit records are currently requested |
| `vm.exit_rip.requested`, `vm.exit_rip.supported` | Recorder debug selection and kernel sync-register support |
| `vm.timers` | Hyper-V MSRs and SIMP timer headers, LAPIC, PIT, PIC, IOAPIC, HPET |
| `vm.pci_bars`, `vm.mch_smram`, `vm.usb_attachments` | Cheap device summaries |
| `process_status`, `thread_count`, `threads` | Linux memory, scheduling, and thread statistics |

Exit records contain `unix_ns`, `host_tsc`, numeric `KVM_EXIT_*`
reason, `rip`, `address`, `size`, `write`, and `errno`. `address` is a port or
MMIO address; `size` is the access width. No I/O payload is retained.
An interrupted or failed ioctl uses reason 4294967295 and carries `errno`.
Per-exit `rip` is null in metadata mode. When exact RIP is enabled on a supporting
kernel, `KVM_SYNC_X86_REGS` copies general registers through the shared KVM
mapping, without an extra ioctl per exit. Older kernels retain null RIP.
The `regs` and `sregs` report fields are read independently of exit recording.

On hosts with an invariant TSC selected as Linux's current clocksource,
recording uses an unfenced `RDTSC`. Its sample is approximate, not an instruction
ordering barrier. First enable calibrates the host counter against `Instant`
over at least 20 ms, outside the steady-state exit loop. At dump time,
`unix_ns` is estimated from the calibrated frequency and a fresh wall-clock
anchor; `host_tsc` preserves the raw sample. Clock steps, calibration drift,
host suspend, or migration can affect the estimate. Repeated dumps can estimate
different Unix times for unchanged history. Sequence order defines exit order.

If Linux stops using TSC, the counter appears ahead of the dump anchor, or
conversion overflows, `unix_ns` is null and raw `host_tsc` remains available.
If TSC is unsuitable at first enable, `exit_clock.kind` is `system_time`:
records use `SystemTime` per exit, retain `unix_ns` directly, and have null
`host_tsc`. This compatibility mode costs more. Clock selection is process-wide;
disabling recording does not recalibrate it. Neither clock path runs while the
recorder is off.

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

On the development host, 2,000,000 iterations measured 0.33 ns per baseline
iteration and 9.36 ns with exit decoding, a counter read, and ring insertion,
a 9.04 ns delta. Earlier optimized runs measured 7.13 and 7.55 ns deltas, compared
with 30.20 ns for the previous mutex plus `SystemTime` implementation. These
are userspace hook measurements with synthetic KVM metadata. They exclude
kernel register-copy cost, concurrent dump readers, and guest throughput.

Profiling controls in the previous KVM loop measured 3.24% to 3.92% for
`SystemTime` alone, against 4.14% to 4.41% for its metadata ring. The new
optional path replaces that per-exit clock read with `RDTSC` where supported
and replaces the mutex with a single-writer atomic-store ring. Remaining
recording work is metadata decoding, a counter read, generation publication,
and cache-line stores. Exact RIP additionally copies kernel general registers.
The default-off path does none of that recording work.

The real KVM loop benchmark compares a bypass baseline, actual recorder-off,
opt-in metadata, opt-in exact RIP, and wall-clock-only and counter-only controls
without ring insertion. It includes the cached-mode check in off and recording
modes and kernel register-copy cost only in RIP mode. It requires
`/dev/kvm`, sync-register support, and a memory-capped scope. It creates a KVM
VM inside the test binary, without a `vmlab-hv` or `qemu-system` process.
The host's test-window policy must permit in-process KVM tests. Its command is:

```sh
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  env CARGO_BUILD_JOBS=4 RUST_TEST_THREADS=1 taskset -c 14 \
  cargo test --release -p vmlab-hv-vmm \
  vcpu::tests::recorder_kvm_loop_overhead -- --ignored --nocapture
```

Three repetitions on CPU 14 discarded a warm-up group and used nine groups
of 200,000 exits per sample. Each mode sample was bracketed by two bypass
baseline samples; its paired delta uses their mean. Mode order reverses each
group, and all modes use one ioctl call site to avoid loop-layout differences.
The guest repeatedly wrote one byte to port 80h. Tests assert a clear sync-reg
mask in off and metadata modes, null RIP in metadata, and populated RIP with
the sync mask set in RIP mode. Linux selected invariant TSC at about 4.292 GHz.

| Mode | Paired median, run 1 | Run 2 | Run 3 | Individual paired range, all runs |
| --- | ---: | ---: | ---: | ---: |
| Default recorder off | +1.51% | -3.59% | -8.46% | -40.44% to +53.97% |
| Opt-in metadata | -3.87% | +1.75% | +2.30% | -25.10% to +79.92% |
| Opt-in exact RIP | -1.33% | -0.04% | +3.36% | -15.45% to +32.98% |
| Wall-clock-only control | +0.26% | +3.25% | +3.24% | -18.82% to +36.21% |
| Counter-only control | +0.27% | -1.02% | +0.10% | -21.17% to +18.76% |

Default-off median baseline/sample times were 3977.85/4037.98,
4275.18/4121.63, and 4144.74/3570.49 ns per exit. The median paired deltas
were +60.13, -153.55, and -468.85 ns. A median of paired differences need not
equal the difference of independently calculated medians.

These shared-host runs had heavy concurrent work, including CPU 14's SMT
sibling. The second and third repetitions also overlapped CI on CPU 0-7.
Their noise floor is much larger than 1%; negative deltas are noise, not
speedups. No consistent positive default-off overhead is resolved, but these
measurements cannot certify a sub-1% bound or establish a stable KVM recording
cost floor. Since the optional ring did not demonstrate a reliable sub-1%
result, the whole per-exit recorder is off by default, as the owner requested.
The enabled-path userspace floor measured about 7 to 9 ns for counter sampling,
metadata decoding, and stores. It does not establish a whole-loop bound.
Representative guest throughput has not been measured.

The seven S1 diagnostics tests cover guest poweroff, guest reset,
SIGTERM, runtime failure, filter changes, on-demand reports, quit, and
startup-error redaction. They also check startup-off with no allocated history,
startup debug, runtime metadata and RIP modes, frozen history after disabling,
unchanged selection after an invalid filter, converted timestamps, and crash-time
register reads for both vCPUs with the recorder off.

### Illustrative report excerpt

This metadata-enabled excerpt omits large arrays and is not a complete captured
report. With the default filter and no prior enable, `exits` and `exit_clock`
would be null and `exit_recorder.enabled` would be false:

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
  "vm": {"exit_recorder": {"enabled": true},
    "vcpus": [{"id": 0,
    "exit_clock": {"kind": "invariant_tsc", "hz": 4291912000}, "exits": [
    {"unix_ns": 1793491200000000000, "host_tsc": 123456789000000,
     "reason": 2, "rip": null,
     "address": 1016, "size": 1, "write": true, "errno": null}
  ]}]}
}
```
