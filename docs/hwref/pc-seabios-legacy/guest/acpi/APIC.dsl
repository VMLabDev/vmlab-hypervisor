/*
 * Intel ACPI Component Architecture
 * AML/ASL+ Disassembler version 20230628 (64-bit version)
 * Copyright (c) 2000 - 2023 Intel Corporation
 * 
 * Disassembly of ../acpi/APIC, Fri Oct 30 20:12:28 2026
 *
 * ACPI Data Table [APIC]
 *
 * Format: [HexOffset DecimalOffset ByteLength]  FieldName : FieldValue (in hex)
 */

[000h 0000 004h]                   Signature : "APIC"    [Multiple APIC Description Table (MADT)]
[004h 0004 004h]                Table Length : 00000078
[008h 0008 001h]                    Revision : 03
[009h 0009 001h]                    Checksum : 88
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

[034h 0052 001h]               Subtable Type : 01 [I/O APIC]
[035h 0053 001h]                      Length : 0C
[036h 0054 001h]                 I/O Apic ID : 00
[037h 0055 001h]                    Reserved : 00
[038h 0056 004h]                     Address : FEC00000
[03Ch 0060 004h]                   Interrupt : 00000000

[040h 0064 001h]               Subtable Type : 02 [Interrupt Source Override]
[041h 0065 001h]                      Length : 0A
[042h 0066 001h]                         Bus : 00
[043h 0067 001h]                      Source : 00
[044h 0068 004h]                   Interrupt : 00000002
[048h 0072 002h]       Flags (decoded below) : 0000
                                    Polarity : 0
                                Trigger Mode : 0

[04Ah 0074 001h]               Subtable Type : 02 [Interrupt Source Override]
[04Bh 0075 001h]                      Length : 0A
[04Ch 0076 001h]                         Bus : 00
[04Dh 0077 001h]                      Source : 05
[04Eh 0078 004h]                   Interrupt : 00000005
[052h 0082 002h]       Flags (decoded below) : 000D
                                    Polarity : 1
                                Trigger Mode : 3

[054h 0084 001h]               Subtable Type : 02 [Interrupt Source Override]
[055h 0085 001h]                      Length : 0A
[056h 0086 001h]                         Bus : 00
[057h 0087 001h]                      Source : 09
[058h 0088 004h]                   Interrupt : 00000009
[05Ch 0092 002h]       Flags (decoded below) : 000D
                                    Polarity : 1
                                Trigger Mode : 3

[05Eh 0094 001h]               Subtable Type : 02 [Interrupt Source Override]
[05Fh 0095 001h]                      Length : 0A
[060h 0096 001h]                         Bus : 00
[061h 0097 001h]                      Source : 0A
[062h 0098 004h]                   Interrupt : 0000000A
[066h 0102 002h]       Flags (decoded below) : 000D
                                    Polarity : 1
                                Trigger Mode : 3

[068h 0104 001h]               Subtable Type : 02 [Interrupt Source Override]
[069h 0105 001h]                      Length : 0A
[06Ah 0106 001h]                         Bus : 00
[06Bh 0107 001h]                      Source : 0B
[06Ch 0108 004h]                   Interrupt : 0000000B
[070h 0112 002h]       Flags (decoded below) : 000D
                                    Polarity : 1
                                Trigger Mode : 3

[072h 0114 001h]               Subtable Type : 04 [Local APIC NMI]
[073h 0115 001h]                      Length : 06
[074h 0116 001h]                Processor ID : FF
[075h 0117 002h]       Flags (decoded below) : 0000
                                    Polarity : 0
                                Trigger Mode : 0
[077h 0119 001h]        Interrupt Input LINT : 01

Raw Table Data: Length 120 (0x78)

    0000: 41 50 49 43 78 00 00 00 03 88 42 4F 43 48 53 20  // APICx.....BOCHS 
    0010: 42 58 50 43 20 20 20 20 01 00 00 00 42 58 50 43  // BXPC    ....BXPC
    0020: 01 00 00 00 00 00 E0 FE 01 00 00 00 00 08 00 00  // ................
    0030: 01 00 00 00 01 0C 00 00 00 00 C0 FE 00 00 00 00  // ................
    0040: 02 0A 00 00 02 00 00 00 00 00 02 0A 00 05 05 00  // ................
    0050: 00 00 0D 00 02 0A 00 09 09 00 00 00 0D 00 02 0A  // ................
    0060: 00 0A 0A 00 00 00 0D 00 02 0A 00 0B 0B 00 00 00  // ................
    0070: 0D 00 04 06 FF 00 00 01                          // ........
