/*
 * Intel ACPI Component Architecture
 * AML/ASL+ Disassembler version 20230628 (64-bit version)
 * Copyright (c) 2000 - 2023 Intel Corporation
 * 
 * Disassembly of ../acpi/APIC, Fri Oct 30 10:12:18 2026
 *
 * ACPI Data Table [APIC]
 *
 * Format: [HexOffset DecimalOffset ByteLength]  FieldName : FieldValue (in hex)
 */

[000h 0000 004h]                   Signature : "APIC"    [Multiple APIC Description Table (MADT)]
[004h 0004 004h]                Table Length : 00000090
[008h 0008 001h]                    Revision : 03
[009h 0009 001h]                    Checksum : 49
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

[03Ch 0060 001h]               Subtable Type : 00 [Processor Local APIC]
[03Dh 0061 001h]                      Length : 08
[03Eh 0062 001h]                Processor ID : 02
[03Fh 0063 001h]               Local Apic ID : 02
[040h 0064 004h]       Flags (decoded below) : 00000001
                           Processor Enabled : 1
                      Runtime Online Capable : 0

[044h 0068 001h]               Subtable Type : 00 [Processor Local APIC]
[045h 0069 001h]                      Length : 08
[046h 0070 001h]                Processor ID : 03
[047h 0071 001h]               Local Apic ID : 03
[048h 0072 004h]       Flags (decoded below) : 00000001
                           Processor Enabled : 1
                      Runtime Online Capable : 0

[04Ch 0076 001h]               Subtable Type : 01 [I/O APIC]
[04Dh 0077 001h]                      Length : 0C
[04Eh 0078 001h]                 I/O Apic ID : 00
[04Fh 0079 001h]                    Reserved : 00
[050h 0080 004h]                     Address : FEC00000
[054h 0084 004h]                   Interrupt : 00000000

[058h 0088 001h]               Subtable Type : 02 [Interrupt Source Override]
[059h 0089 001h]                      Length : 0A
[05Ah 0090 001h]                         Bus : 00
[05Bh 0091 001h]                      Source : 00
[05Ch 0092 004h]                   Interrupt : 00000002
[060h 0096 002h]       Flags (decoded below) : 0000
                                    Polarity : 0
                                Trigger Mode : 0

[062h 0098 001h]               Subtable Type : 02 [Interrupt Source Override]
[063h 0099 001h]                      Length : 0A
[064h 0100 001h]                         Bus : 00
[065h 0101 001h]                      Source : 05
[066h 0102 004h]                   Interrupt : 00000005
[06Ah 0106 002h]       Flags (decoded below) : 000D
                                    Polarity : 1
                                Trigger Mode : 3

[06Ch 0108 001h]               Subtable Type : 02 [Interrupt Source Override]
[06Dh 0109 001h]                      Length : 0A
[06Eh 0110 001h]                         Bus : 00
[06Fh 0111 001h]                      Source : 09
[070h 0112 004h]                   Interrupt : 00000009
[074h 0116 002h]       Flags (decoded below) : 000D
                                    Polarity : 1
                                Trigger Mode : 3

[076h 0118 001h]               Subtable Type : 02 [Interrupt Source Override]
[077h 0119 001h]                      Length : 0A
[078h 0120 001h]                         Bus : 00
[079h 0121 001h]                      Source : 0A
[07Ah 0122 004h]                   Interrupt : 0000000A
[07Eh 0126 002h]       Flags (decoded below) : 000D
                                    Polarity : 1
                                Trigger Mode : 3

[080h 0128 001h]               Subtable Type : 02 [Interrupt Source Override]
[081h 0129 001h]                      Length : 0A
[082h 0130 001h]                         Bus : 00
[083h 0131 001h]                      Source : 0B
[084h 0132 004h]                   Interrupt : 0000000B
[088h 0136 002h]       Flags (decoded below) : 000D
                                    Polarity : 1
                                Trigger Mode : 3

[08Ah 0138 001h]               Subtable Type : 04 [Local APIC NMI]
[08Bh 0139 001h]                      Length : 06
[08Ch 0140 001h]                Processor ID : FF
[08Dh 0141 002h]       Flags (decoded below) : 0000
                                    Polarity : 0
                                Trigger Mode : 0
[08Fh 0143 001h]        Interrupt Input LINT : 01

Raw Table Data: Length 144 (0x90)

    0000: 41 50 49 43 90 00 00 00 03 49 42 4F 43 48 53 20  // APIC.....IBOCHS 
    0010: 42 58 50 43 20 20 20 20 01 00 00 00 42 58 50 43  // BXPC    ....BXPC
    0020: 01 00 00 00 00 00 E0 FE 01 00 00 00 00 08 00 00  // ................
    0030: 01 00 00 00 00 08 01 01 01 00 00 00 00 08 02 02  // ................
    0040: 01 00 00 00 00 08 03 03 01 00 00 00 01 0C 00 00  // ................
    0050: 00 00 C0 FE 00 00 00 00 02 0A 00 00 02 00 00 00  // ................
    0060: 00 00 02 0A 00 05 05 00 00 00 0D 00 02 0A 00 09  // ................
    0070: 09 00 00 00 0D 00 02 0A 00 0A 0A 00 00 00 0D 00  // ................
    0080: 02 0A 00 0B 0B 00 00 00 0D 00 04 06 FF 00 00 01  // ................
