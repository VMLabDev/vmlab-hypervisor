# Phase 4a source exposure audit

During read-only follow-up diagnosis, a sub-agent ran a broad web search whose
results displayed QEMU C implementation snippets. The agent did not open the
source pages. Its context nevertheless contained implementation text; a claim
that the session never saw QEMU C would be inaccurate.

The agent reported six search results in three groups:

- F1: `hw/isa/piix3.c`, concerning configuration writes, routing, and reset.
- F2: older `hw/piix_pci.c`, concerning PCI identity, reset, and configuration.
- F3: `hw/acpi/piix4.c`, concerning legacy I/O inventory and configuration.

No implementation text is reproduced here. No QEMU code was copied or
translated into this project.

The agent reported that all its implementation edits preceded the exposure.
Its i440fx changes and tests used the parent session's independently collected
QEMU qtest JSON fixtures. Those fixtures exercise complete 256-byte reset and
write sequences; the resulting Rust model is checked against those bytes.
The other device-model agents and the parent did not receive the C snippets.

The exposed agent performed no subsequent implementation work. Its later
diagnostic findings are accepted only after independent verification. The
parent verified Linux's passive-release fixup directly in Linux v6.12:
https://raw.githubusercontent.com/torvalds/linux/v6.12/drivers/pci/quirks.c

Legacy I/O inventory and register behavior are verified using the executable
QEMU qtest oracle and archived JSON, rather than implementation source.
This record documents the exposure and engineering provenance; it does not
make a licensing determination.
