# macOS CPU capture

`cpuid.json` records QEMU 11.1.2 on this host with four vCPUs and the exact
macOS profile CPU string. The Linux probe pins itself to vCPU 0 before
enumerating basic, extended, hypervisor, cache, topology, and XSAVE subleaves.
The kernel hash and TSC pin are recorded in the JSON.

Reproduction command:

```
python3 scripts/hwref/macos-cpuid.py --tsc-khz 4291934 --output docs/hwref/q35-macos-tahoe/cpuid.json
```

The probe holds a shared VM permit, counts project VMs before boot, and runs
QEMU under a 24-GiB systemd scope with no swap. It uses direct Linux syscalls
because this workstation's static glibc requires instructions absent from
the Skylake model. The first two probes failed with an invalid opcode in
glibc before executing the dumper. Both exited and were reaped.

The runtime model masks HLE and RTM, retains the Intel vendor and KVM
signature, and adjusts APIC IDs, cache sharing, and topology for the requested
vCPU count. KVM controls OSXSAVE according to guest CR4.

The QEMU frequency leaf reports TSC kHz in EAX and the local APIC bus rate,
1,000,000 kHz, in EBX. vmlab-hv uses the actual `KVM_GET_TSC_KHZ` result in
EAX and the captured APIC rate in EBX. The owner corrected the original
specification, which incorrectly required both fields to contain TSC kHz.
