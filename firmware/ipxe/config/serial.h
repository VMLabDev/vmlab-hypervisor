/* q35 has a fixed COM1 UART and does not require an ACPI SPCR table. */
#undef SERIAL_SPCR
#define SERIAL_FIXED
