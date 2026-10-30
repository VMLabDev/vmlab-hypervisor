/*
 * Intel ACPI Component Architecture
 * AML/ASL+ Disassembler version 20230628 (64-bit version)
 * Copyright (c) 2000 - 2023 Intel Corporation
 * 
 * Disassembly of ../acpi/WAET, Fri Oct 30 16:39:23 2026
 *
 * ACPI Data Table [WAET]
 *
 * Format: [HexOffset DecimalOffset ByteLength]  FieldName : FieldValue (in hex)
 */

[000h 0000 004h]                   Signature : "WAET"    [Windows ACPI Emulated Devices Table]
[004h 0004 004h]                Table Length : 00000028
[008h 0008 001h]                    Revision : 01
[009h 0009 001h]                    Checksum : 39
[00Ah 0010 006h]                      Oem ID : "BOCHS "
[010h 0016 008h]                Oem Table ID : "BXPC    "
[018h 0024 004h]                Oem Revision : 00000001
[01Ch 0028 004h]             Asl Compiler ID : "BXPC"
[020h 0032 004h]       Asl Compiler Revision : 00000001

[024h 0036 004h]       Flags (decoded below) : 00000002
                        RTC needs no INT ack : 0
                     PM timer, one read only : 1

Raw Table Data: Length 40 (0x28)

    0000: 57 41 45 54 28 00 00 00 01 39 42 4F 43 48 53 20  // WAET(....9BOCHS 
    0010: 42 58 50 43 20 20 20 20 01 00 00 00 42 58 50 43  // BXPC    ....BXPC
    0020: 01 00 00 00 02 00 00 00                          // ........
