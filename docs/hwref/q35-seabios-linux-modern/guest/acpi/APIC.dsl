/*
 * Intel ACPI Component Architecture
 * AML/ASL+ Disassembler version 20230628 (64-bit version)
 * Copyright (c) 2000 - 2023 Intel Corporation
 * 
 * Disassembly of ../acpi/APIC, Fri Oct 30 14:20:31 2026
 *
 * ACPI Data Table [APIC]
 *
 * Format: [HexOffset DecimalOffset ByteLength]  FieldName : FieldValue (in hex)
 */

[000h 0000 004h]                   Signature : "APIC"    [Multiple APIC Description Table (MADT)]
[004h 0004 004h]                Table Length : 00000080
[008h 0008 001h]                    Revision : 03
[009h 0009 001h]                    Checksum : 75
[00Ah 0010 006h]                      Oem ID : "BOCHS "
[010h 0016 008h]                Oem Table ID : "BXPC    "
[018h 0024 004h]                Oem Revision : 00000001
[01Ch 0028 004h]             Asl Compiler ID : "BXPC"
[020h 0032 004h]       Asl Compiler Revision : 00000001

[024h 0036 004h]          Local Apic Address : FEE00000
[028h 0040 004h]       Flags (decoded below) : 00000001
                         PC-AT Compatibility : 1

[02Ch 0044 001h]               Subtable Type : 00 [Processor Local APIC]
[02Dh 0045 001h]                      Length : 08
[02Eh 0046 001h]                Processor ID : 00
[02Fh 0047 001h]               Local Apic ID : 00
[030h 0048 004h]       Flags (decoded below) : 00000001
                           Processor Enabled : 1
                      Runtime Online Capable : 0

[034h 0052 001h]               Subtable Type : 00 [Processor Local APIC]
[035h 0053 001h]                      Length : 08
[036h 0054 001h]                Processor ID : 01
[037h 0055 001h]               Local Apic ID : 01
[038h 0056 004h]       Flags (decoded below) : 00000001
                           Processor Enabled : 1
                      Runtime Online Capable : 0

[03Ch 0060 001h]               Subtable Type : 01 [I/O APIC]
[03Dh 0061 001h]                      Length : 0C
[03Eh 0062 001h]                 I/O Apic ID : 00
[03Fh 0063 001h]                    Reserved : 00
[040h 0064 004h]                     Address : FEC00000
[044h 0068 004h]                   Interrupt : 00000000

[048h 0072 001h]               Subtable Type : 02 [Interrupt Source Override]
[049h 0073 001h]                      Length : 0A
[04Ah 0074 001h]                         Bus : 00
[04Bh 0075 001h]                      Source : 00
[04Ch 0076 004h]                   Interrupt : 00000002
[050h 0080 002h]       Flags (decoded below) : 0000
                                    Polarity : 0
                                Trigger Mode : 0

[052h 0082 001h]               Subtable Type : 02 [Interrupt Source Override]
[053h 0083 001h]                      Length : 0A
[054h 0084 001h]                         Bus : 00
[055h 0085 001h]                      Source : 05
[056h 0086 004h]                   Interrupt : 00000005
[05Ah 0090 002h]       Flags (decoded below) : 000D
                                    Polarity : 1
                                Trigger Mode : 3

[05Ch 0092 001h]               Subtable Type : 02 [Interrupt Source Override]
[05Dh 0093 001h]                      Length : 0A
[05Eh 0094 001h]                         Bus : 00
[05Fh 0095 001h]                      Source : 09
[060h 0096 004h]                   Interrupt : 00000009
[064h 0100 002h]       Flags (decoded below) : 000D
                                    Polarity : 1
                                Trigger Mode : 3

[066h 0102 001h]               Subtable Type : 02 [Interrupt Source Override]
[067h 0103 001h]                      Length : 0A
[068h 0104 001h]                         Bus : 00
[069h 0105 001h]                      Source : 0A
[06Ah 0106 004h]                   Interrupt : 0000000A
[06Eh 0110 002h]       Flags (decoded below) : 000D
                                    Polarity : 1
                                Trigger Mode : 3

[070h 0112 001h]               Subtable Type : 02 [Interrupt Source Override]
[071h 0113 001h]                      Length : 0A
[072h 0114 001h]                         Bus : 00
[073h 0115 001h]                      Source : 0B
[074h 0116 004h]                   Interrupt : 0000000B
[078h 0120 002h]       Flags (decoded below) : 000D
                                    Polarity : 1
                                Trigger Mode : 3

[07Ah 0122 001h]               Subtable Type : 04 [Local APIC NMI]
[07Bh 0123 001h]                      Length : 06
[07Ch 0124 001h]                Processor ID : FF
[07Dh 0125 002h]       Flags (decoded below) : 0000
                                    Polarity : 0
                                Trigger Mode : 0
[07Fh 0127 001h]        Interrupt Input LINT : 01

Raw Table Data: Length 128 (0x80)

    0000: 41 50 49 43 80 00 00 00 03 75 42 4F 43 48 53 20  // APIC.....uBOCHS 
    0010: 42 58 50 43 20 20 20 20 01 00 00 00 42 58 50 43  // BXPC    ....BXPC
    0020: 01 00 00 00 00 00 E0 FE 01 00 00 00 00 08 00 00  // ................
    0030: 01 00 00 00 00 08 01 01 01 00 00 00 01 0C 00 00  // ................
    0040: 00 00 C0 FE 00 00 00 00 02 0A 00 00 02 00 00 00  // ................
    0050: 00 00 02 0A 00 05 05 00 00 00 0D 00 02 0A 00 09  // ................
    0060: 09 00 00 00 0D 00 02 0A 00 0A 0A 00 00 00 0D 00  // ................
    0070: 02 0A 00 0B 0B 00 00 00 0D 00 04 06 FF 00 00 01  // ................
