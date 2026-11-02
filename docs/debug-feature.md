# Debug feature

This reference describes whole-VM debugging for x86 KVM guests. The source
inventory is `vmlab-hv-proto::debug::DebugRequest` and `DebugResponse`,
`vmlab-hv-vmm::debug`, and the optional `vmlab-hv-proto::gdb` bridge.
The existing control socket carries all guest operations. No guest agent is needed.

## Control protocol

The handshake advertises `debug` when KVM supports guest debugging.
The protocol major remains 1. A client checks the capability before sending
`command: "debug"`. Existing commands keep their existing reply formats.

Example request, after the version handshake:

```json
{"type":"request","id":1,"command":{"command":"debug","request":{"operation":"pause"}}}
```

Successful requests receive `outcome: "debug"` and a typed `response` object.
Its `result` is `status`, `memory`, `matches`, `registers`, or `done`.
Failures use the existing `outcome: "error"`, `code`, and `message` fields.

| Operation | Fields | Result and conditions |
| --- | --- | --- |
| `status` | None | Run state, stepping flag, vCPU IDs, breakpoints, and the latest stop |
| `pause` | None | Stops all vCPUs, completes pending KVM exits, enables debugging, and returns status |
| `continue` | None | Resumes all vCPUs and retains hardware breakpoints |
| `continue_vcpus` | `vcpus: [u8]` | Resumes the selected vCPUs and keeps the others stopped |
| `step` | `vcpu: u8` | Starts one instruction on that vCPU. Other vCPUs remain stopped. Completion is asynchronous |
| `detach` | `resume: bool` | Removes debugger settings and breakpoints. Optionally resumes execution. Guest edits remain |
| `read_memory` | `space`, `address: u64`, `length: usize` | Returns `data: [u8]` |
| `write_memory` | `space`, `address: u64`, `data: [u8]` | Changes guest RAM and returns `done` |
| `search_memory` | `space`, `address: u64`, `length: usize`, `pattern: [u8]` | Returns matching addresses and a `truncated` flag |
| `read_registers` | `vcpu: u8` | Returns that vCPU's register state |
| `write_registers` | `vcpu: u8`, `registers` | Replaces the supported register state |
| `set_registers` | `vcpu: u8`, `values: {name: u64}` | Applies named register edits |
| `add_breakpoint` | `breakpoint` | Adds a hardware breakpoint to every vCPU. Repeating an identical breakpoint is idempotent |
| `remove_breakpoint` | `breakpoint` | Removes the matching breakpoint. Removing an absent breakpoint is idempotent |

Except for `status`, `pause`, and `continue`, operations require a paused VM
with no step in flight. Memory and register operations wait for every vCPU
to park. A suspended guest must be resumed before debugging.

`step` returns before its instruction necessarily completes. A client observes
`debug_stopped`, or polls `status` for a newer stop sequence and `stepping: false`.
An ordinary `pause` cancels an in-flight step. An ordinary `resume` releases
all vCPUs, including those held by debugger scheduler locking.

Each stop reports a monotonic sequence, vCPU ID, linear PC, and reason.
Reasons distinguish pause, step, breakpoint, software breakpoint, and other
debug exceptions. A breakpoint stop includes the matching breakpoint.
Hardware traps stop the VM and emit `stopped` followed by `debug_stopped`.
An explicit debugger pause emits `debug_stopped` even if already paused.

## Memory

`space` is `{"kind":"physical"}` or `{"kind":"virtual","vcpu":0}`.
Virtual addresses are linear addresses translated with the selected vCPU's
current page tables and paging mode. They do not select an OS process.

Reads and writes accept 1 through 65,536 bytes per request. A search accepts
1 through 16,777,216 bytes and a pattern of 1 through 65,536 bytes.
Searches include overlapping matches and matches spanning transfer boundaries.
Results stop after 1,024 matches. `truncated: true` means another match exists.

Reads cover mapped RAM and firmware ROM, including the legacy firmware alias.
Writes cover RAM in the machine's memory layout. Writes do not change firmware
ROM, the legacy VGA or shadow aperture, PCI holes, or MMIO devices.
The complete range is checked before a write changes any byte.
Unmapped virtual pages and invalid physical ranges fail without a partial write.
Writes through valid translations can modify guest read-only pages, as a debugger
can modify executable code.

All-stop applies to vCPUs. Host device workers and passthrough DMA can still
change RAM. Hardware watchpoints detect CPU accesses, not DMA or host writes.
An external device's state is not frozen by the debugger.

## Registers

The register result contains the general registers in `GENERAL_REGISTERS`
order, segment selectors in `SEGMENT_REGISTERS` order, x87 state, XMM registers,
MXCSR, and named system registers. AVX upper halves, AVX-512, arbitrary MSRs,
and descriptor-table editing are not exposed.

Named edits support `rax` through `r15`, `rbp`, `rsp`, `rip`, `rflags`,
segment selectors, `cr0`, `cr2`, `cr3`, `cr4`, `cr8`, `efer`, `fs_base`, and
`gs_base`. Code and stack bases and `cs_long` are read-only metadata.
Changing a protected-mode segment selector is refused because that requires
loading its descriptor. Real-mode selector edits also update their bases.
Invalid names are refused before any register changes.

## Breakpoints

A breakpoint contains `address: u64`, `length: u8`, and `kind`:
`execute`, `write`, or `read_write`. Addresses are linear, in whichever
address space a vCPU is executing. Breakpoints are installed on every vCPU.
There is no PID, CR3, or process filter.

Four hardware slots are available per VM. Execution breakpoints require
length 1. Data watchpoints accept aligned lengths 1, 2, 4, or 8.
x86 does not provide a read-only data watchpoint. `read_write` detects either
access. Unsupported lengths, misalignment, and a fifth breakpoint are refused.
Removing an entry can renumber the slots shown by `status`.

Debugger settings belong to the host and are not snapshot sections. Hardware
breakpoints remain configured across a guest snapshot load. Detaching clears
the debugger settings without replacing the guest's architectural debug registers.
While debugging is enabled, guest debug exceptions can also stop the VM.

## GDB and LLDB

The protocol crate's optional `gdb` feature provides a Remote Serial Protocol
bridge over an accepted TCP connection. vmlab exposes it through
`vmlab debug <machine> gdb`. Its listener accepts loopback addresses only.
It has no authentication. A local process able to connect can control the VM.
The bridge stops the VM on attachment and presents each vCPU as a thread,
with thread ID equal to vCPU ID plus one. It supports registers, virtual-memory
access, breakpoints, watchpoints, stepping, interrupts, and scheduler locking.

Ordinary debugger breakpoints and explicit hardware breakpoints use the same
four slots. The bridge does not patch guest instructions. In segmented modes,
GDB's PC is linear. The native register result retains architectural RIP and
reports the code base separately.
The register description is x86-64 with SSE. Automatic disassembly-mode switching
for 16-bit or 32-bit code is not provided.

Clean detach removes debugger settings and resumes the VM. Connection loss,
a debugger kill request, or interrupting the vmlab bridge leaves the VM paused
after removing debugger settings. Intentional guest memory and register edits
remain. Use one controlling debugger session per VM.
If the VM starts shutting down, the bridge ends the session without inventing
a target exit code. The hypervisor's final process outcome remains authoritative.

## Errors and verification

`invalid_state` means the operation needs a paused VM or the machine is exiting.
`invalid_argument` covers invalid IDs, ranges, register edits, and breakpoint
parameters. `unsupported` means the host lacks KVM guest-debug support.
`internal` reports a failed KVM operation or stop barrier.

`crates/vmlab-hv/tests/debug.rs` exercises the real control socket, memory
translation, write preflight, register updates, hardware traps, SMP stepping,
high-address long-mode execution and eight-byte watchpoints,
remote interruption, disconnect cleanup, and real debugger clients.
The LLDB client check skips with its startup error when LLDB cannot run.
The long-mode fixture requires `nasm` and skips with a named reason if absent.
`crates/vmlab-hv-proto/tests/debug.rs` checks message shapes and malformed input.
The vmlab checkout's `tests/cli_debug.rs` covers the CLI and GDB bridge through
its typed lab socket with `VMLAB_HV_DEBUG_TEST_BINARY` naming the built hypervisor.

KVM's debugger and translation contracts are documented at
https://docs.kernel.org/virt/kvm/api.html#kvm-set-guest-debug and
https://docs.kernel.org/virt/kvm/api.html#kvm-translate.
