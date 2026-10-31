# platform observations

Counts cover the full trace stream, including lines omitted from the bounded raw log.
Addresses retain QEMU trace notation. Use `mtree.txt` to interpret regions and aliases.
A range in the value column includes every integer between its endpoints.
Aperture rows combine framebuffer byte addresses; they are not indexed register decodes.
Physical-memory snapshots can add observer reads to legacy VGA aperture rows.
Those aperture counts are not guest-only counts. Host fw_cfg inventory reads are excluded.
First use is the 1-based line number across all enabled events, including firmware.

## Register accesses

| Region or event | Address or index | Operation | Bytes | Count | First use | Values |
|---|---|---|---:|---:|---:|---|
| `port92` | `0x92` | read | 1 | 6033 | 18 | 0x0, 0x2 |
| `port92` | `0x92` | write | 1 | 3 | 19 | 0x2 |
| `pci-conf-idx` | `0xcf8` | write | 4 | 430 | 20 | 0x80000000, 0x80000004, 0x80000008, 0x8000000c, 0x80000010, 0x80000014, 0x80000018, 0x8000001c, 0x80000020, 0x80000024, 0x8000002c, 0x80000030, 0x8000003c, 0x80000058, 0x8000005c, 0x80000070, 0x80000800, 0x80000804, 0x80000808, 0x8000080c, 0x80000810, 0x80000814, 0x80000818, 0x8000081c, 0x80000820, 0x80000824, 0x80000830, 0x8000083c, 0x80000860, 0x80000900, 0x80000904, 0x80000908, 0x8000090c, 0x80000910, 0x80000914, 0x80000918, 0x8000091c, 0x80000920, 0x80000924, 0x80000930, 0x8000093c, 0x80000940, 0x80000a00, 0x80000a04, 0x80000a08, 0x80000a0c, 0x80000a10, 0x80000a14, 0x80000a18, 0x80000a1c, 0x80000a20, 0x80000a24, 0x80000a30, 0x80000a3c, 0x80000ac0, 0x80000b00, 0x80000b04, 0x80000b08, 0x80000b0c, 0x80000b10, 0x80000b14, 0x80000b18, 0x80000b1c, 0x80000b20, 0x80000b24, 0x80000b30, 0x80000b3c, 0x80000b40, 0x80000b58, 0x80000b80, 0x80000b90, 0x80000bd0, 0x80000c00, 0x80000d00, 0x80000e00, 0x80000f00, 0x80001000, 0x80001004, 0x80001008, 0x8000100c, 0x80001010, 0x80001014, 0x80001018, 0x8000101c, 0x80001020, 0x80001024, 0x80001030, 0x8000103c, 0x80001800, 0x80002000, 0x80002800, 0x80003000, 0x80003800, 0x80004000, 0x80004800, 0x80005000, 0x80005800, 0x80006000, 0x80006800, 0x80007000, 0x80007800, 0x80008000, 0x80008800, 0x80009000, 0x80009800, 0x8000a000, 0x8000a800, 0x8000b000, 0x8000b800, 0x8000c000, 0x8000c800, 0x8000d000, 0x8000d800, 0x8000e000, 0x8000e800, 0x8000f000, 0x8000f800 |
| `pci-conf-data` | `0xcfc` | read | 2 | 133 | 21 | 0x0, 0x103, 0x1234, 0x1af4, 0x8086, 0xffffffff |
| `pci-conf-data` | `0xcfc` | read | 4 | 115 | 23 | 0x0-0x1, 0x8, 0xc001, 0x1018000, 0x3000002, 0x6000002, 0x6010000, 0x6800003, 0xc030001, 0x11111234, 0x12378086, 0x33333000, 0x33333333, 0x70008086, 0x70108086, 0x70208086, 0x71138086, 0xfd000008, 0xfebe0000, 0xfebf0000, 0xff000008, 0xffff0000, 0xfffff000, 0xffffffe1, 0xfffffff1 |
| `pci-conf-data` | `0xcfd` | read | 1 | 13 | 25 | 0x0-0x1, 0x4 |
| `pci-conf-data` | `0xcfc` | write | 4 | 107 | 31 | 0x0-0x1, 0x8, 0x601, 0x701, 0xc000, 0xc020, 0x2000000, 0x11111000, 0x31111111, 0x33333000, 0x33333333, 0xfd000000, 0xfebe0000-0xfebe0001, 0xfebf0000, 0xfffff800, 0xfffffffe-0xffffffff |
| `pci-conf-data` | `0xcfe` | read | 2 | 17 | 40 | 0x101, 0x300, 0x600-0x601, 0x680, 0xc03, 0x1100, 0x1111, 0x1237, 0x7113 |
| `fwcfg` | `0x510` | write | 2 | 3 | 45 | 0x0-0x1, 0x25 |
| `fwcfg` | `0x511` | read | 1 | 8 | 46 | 0x0, 0x3, 0x45, 0x4d, 0x51, 0x55 |
| `fwcfg.dma` | `0x518` | write | 4 | 41 | 55 | 0x6a9c, 0x6d24, 0x6d2c, 0x6d34, 0x6d38, 0x6d3c, 0x6d48, 0x6d4c, 0x6d50, 0x6d5c, 0x6ddc, 0x6f3c |
| `pci-conf-idx` | `0xcf8` | read | 4 | 1 | 100 | 0x80000000 |
| `pci-conf-data` | `0xcfe` | read | 1 | 15 | 106 | 0x0, 0x80 |
| `pci-conf-data` | `0xcfc` | write | 2 | 10 | 764 | 0x103, 0x107, 0x8000, 0x8f00 |
| `pci-conf-data` | `0xcfc` | write | 1 | 5 | 768 | 0x1, 0x9-0xb |
| `pci-conf-data` | `0xcfd` | write | 1 | 1 | 770 | 0xa |
| `pci-conf-data` | `0xcfe` | write | 1 | 4 | 772 | 0x9-0xb, 0x4a |
| `pci-conf-data` | `0xcff` | write | 1 | 1 | 774 | 0xb |
| `pci-conf-data` | `0xcfe` | write | 2 | 1 | 784 | 0x8000 |
| `io` | `0x628` | read | 4 | 1 | 831 | 0xffffffffffffffff |
| `io` | `0x628` | write | 4 | 1 | 832 | 0xffffffff |
| `apm-io` | `0xb3` | write | 1 | 2 | 833 | 0x0-0x1 |
| `apm-io` | `0xb2` | write | 1 | 3 | 834 | 0x0, 0xb5 |
| `apm-io` | `0xb2` | read | 1 | 3 | 835 | 0x0, 0xb5 |
| `apm-io` | `0xb3` | read | 1 | 1 | 837 | 0x0 |
| `pci-conf-data` | `0xcfc` | read | 1 | 7 | 863 | 0x0, 0x9, 0xb |
| `vbe` | `0x1ce` | write | 2 | 11 | 981 | 0x0-0x1, 0x3-0x4, 0xa |
| `vbe` | `0x1cf` | write | 2 | 6 | 982 | 0x0, 0x2, 0xb0c0, 0xb0c5 |
| `vbe` | `0x1cf` | read | 2 | 5 | 986 | 0x0, 0x20, 0x100, 0x3e80, 0xb0c0 |
| `edid` | `0xfebf0000` | read | 1 | 1 | 1014 | 0x0 |
| `edid` | `0xfebf0001` | read | 1 | 1 | 1015 | 0xff |
| `edid` | `0xfebf0002` | read | 1 | 1 | 1016 | 0xff |
| `edid` | `0xfebf0003` | read | 1 | 1 | 1017 | 0xff |
| `edid` | `0xfebf0004` | read | 1 | 1 | 1018 | 0xff |
| `edid` | `0xfebf0005` | read | 1 | 1 | 1019 | 0xff |
| `edid` | `0xfebf0006` | read | 1 | 1 | 1020 | 0xff |
| `edid` | `0xfebf0007` | read | 1 | 1 | 1021 | 0x0 |
| `edid` | `0xfebf0008` | read | 1 | 1 | 1022 | 0x49 |
| `edid` | `0xfebf0009` | read | 1 | 1 | 1023 | 0x14 |
| `edid` | `0xfebf000a` | read | 1 | 1 | 1024 | 0x34 |
| `edid` | `0xfebf000b` | read | 1 | 1 | 1025 | 0x12 |
| `edid` | `0xfebf000c` | read | 1 | 1 | 1026 | 0x0 |
| `edid` | `0xfebf000d` | read | 1 | 1 | 1027 | 0x0 |
| `edid` | `0xfebf000e` | read | 1 | 1 | 1028 | 0x0 |
| `edid` | `0xfebf000f` | read | 1 | 1 | 1029 | 0x0 |
| `edid` | `0xfebf0010` | read | 1 | 1 | 1030 | 0x2a |
| `edid` | `0xfebf0011` | read | 1 | 1 | 1031 | 0x18 |
| `edid` | `0xfebf0012` | read | 1 | 1 | 1032 | 0x1 |
| `edid` | `0xfebf0013` | read | 1 | 1 | 1033 | 0x4 |
| `edid` | `0xfebf0014` | read | 1 | 1 | 1034 | 0xa5 |
| `edid` | `0xfebf0015` | read | 1 | 1 | 1035 | 0x20 |
| `edid` | `0xfebf0016` | read | 1 | 1 | 1036 | 0x14 |
| `edid` | `0xfebf0017` | read | 1 | 1 | 1037 | 0x78 |
| `edid` | `0xfebf0018` | read | 1 | 1 | 1038 | 0x6 |
| `edid` | `0xfebf0019` | read | 1 | 1 | 1039 | 0xee |
| `edid` | `0xfebf001a` | read | 1 | 1 | 1040 | 0x91 |
| `edid` | `0xfebf001b` | read | 1 | 1 | 1041 | 0xa3 |
| `edid` | `0xfebf001c` | read | 1 | 1 | 1042 | 0x54 |
| `edid` | `0xfebf001d` | read | 1 | 1 | 1043 | 0x4c |
| `edid` | `0xfebf001e` | read | 1 | 1 | 1044 | 0x99 |
| `edid` | `0xfebf001f` | read | 1 | 1 | 1045 | 0x26 |
| `edid` | `0xfebf0020` | read | 1 | 1 | 1046 | 0xf |
| `edid` | `0xfebf0021` | read | 1 | 1 | 1047 | 0x50 |
| `edid` | `0xfebf0022` | read | 1 | 1 | 1048 | 0x54 |
| `edid` | `0xfebf0023` | read | 1 | 1 | 1049 | 0x21 |
| `edid` | `0xfebf0024` | read | 1 | 1 | 1050 | 0x8 |
| `edid` | `0xfebf0025` | read | 1 | 1 | 1051 | 0x0 |
| `edid` | `0xfebf0026` | read | 1 | 1 | 1052 | 0xe1 |
| `edid` | `0xfebf0027` | read | 1 | 1 | 1053 | 0xc0 |
| `edid` | `0xfebf0028` | read | 1 | 1 | 1054 | 0xd1 |
| `edid` | `0xfebf0029` | read | 1 | 1 | 1055 | 0xc0 |
| `edid` | `0xfebf002a` | read | 1 | 1 | 1056 | 0xd1 |
| `edid` | `0xfebf002b` | read | 1 | 1 | 1057 | 0x0 |
| `edid` | `0xfebf002c` | read | 1 | 1 | 1058 | 0xa9 |
| `edid` | `0xfebf002d` | read | 1 | 1 | 1059 | 0x40 |
| `edid` | `0xfebf002e` | read | 1 | 1 | 1060 | 0xb3 |
| `edid` | `0xfebf002f` | read | 1 | 1 | 1061 | 0x0 |
| `edid` | `0xfebf0030` | read | 1 | 1 | 1062 | 0x95 |
| `edid` | `0xfebf0031` | read | 1 | 1 | 1063 | 0x0 |
| `edid` | `0xfebf0032` | read | 1 | 1 | 1064 | 0x81 |
| `edid` | `0xfebf0033` | read | 1 | 1 | 1065 | 0x80 |
| `edid` | `0xfebf0034` | read | 1 | 1 | 1066 | 0x81 |
| `edid` | `0xfebf0035` | read | 1 | 1 | 1067 | 0x40 |
| `edid` | `0xfebf0036` | read | 1 | 1 | 1068 | 0xea |
| `edid` | `0xfebf0037` | read | 1 | 1 | 1069 | 0x29 |
| `edid` | `0xfebf0038` | read | 1 | 1 | 1070 | 0x0 |
| `edid` | `0xfebf0039` | read | 1 | 1 | 1071 | 0xc0 |
| `edid` | `0xfebf003a` | read | 1 | 1 | 1072 | 0x51 |
| `edid` | `0xfebf003b` | read | 1 | 1 | 1073 | 0x20 |
| `edid` | `0xfebf003c` | read | 1 | 1 | 1074 | 0x1c |
| `edid` | `0xfebf003d` | read | 1 | 1 | 1075 | 0x30 |
| `edid` | `0xfebf003e` | read | 1 | 1 | 1076 | 0x40 |
| `edid` | `0xfebf003f` | read | 1 | 1 | 1077 | 0x26 |
| `edid` | `0xfebf0040` | read | 1 | 1 | 1078 | 0x44 |
| `edid` | `0xfebf0041` | read | 1 | 1 | 1079 | 0x40 |
| `edid` | `0xfebf0042` | read | 1 | 1 | 1080 | 0x45 |
| `edid` | `0xfebf0043` | read | 1 | 1 | 1081 | 0xcb |
| `edid` | `0xfebf0044` | read | 1 | 1 | 1082 | 0x10 |
| `edid` | `0xfebf0045` | read | 1 | 1 | 1083 | 0x0 |
| `edid` | `0xfebf0046` | read | 1 | 1 | 1084 | 0x0 |
| `edid` | `0xfebf0047` | read | 1 | 1 | 1085 | 0x18 |
| `edid` | `0xfebf0048` | read | 1 | 1 | 1086 | 0x0 |
| `edid` | `0xfebf0049` | read | 1 | 1 | 1087 | 0x0 |
| `edid` | `0xfebf004a` | read | 1 | 1 | 1088 | 0x0 |
| `edid` | `0xfebf004b` | read | 1 | 1 | 1089 | 0xf7 |
| `edid` | `0xfebf004c` | read | 1 | 1 | 1090 | 0x0 |
| `edid` | `0xfebf004d` | read | 1 | 1 | 1091 | 0xa |
| `edid` | `0xfebf004e` | read | 1 | 1 | 1092 | 0x0 |
| `edid` | `0xfebf004f` | read | 1 | 1 | 1093 | 0x40 |
| `edid` | `0xfebf0050` | read | 1 | 1 | 1094 | 0x82 |
| `edid` | `0xfebf0051` | read | 1 | 1 | 1095 | 0x0 |
| `edid` | `0xfebf0052` | read | 1 | 1 | 1096 | 0x28 |
| `edid` | `0xfebf0053` | read | 1 | 1 | 1097 | 0x20 |
| `edid` | `0xfebf0054` | read | 1 | 1 | 1098 | 0x0 |
| `edid` | `0xfebf0055` | read | 1 | 1 | 1099 | 0x0 |
| `edid` | `0xfebf0056` | read | 1 | 1 | 1100 | 0x0 |
| `edid` | `0xfebf0057` | read | 1 | 1 | 1101 | 0x0 |
| `edid` | `0xfebf0058` | read | 1 | 1 | 1102 | 0x0 |
| `edid` | `0xfebf0059` | read | 1 | 1 | 1103 | 0x0 |
| `edid` | `0xfebf005a` | read | 1 | 1 | 1104 | 0x0 |
| `edid` | `0xfebf005b` | read | 1 | 1 | 1105 | 0x0 |
| `edid` | `0xfebf005c` | read | 1 | 1 | 1106 | 0x0 |
| `edid` | `0xfebf005d` | read | 1 | 1 | 1107 | 0xfd |
| `edid` | `0xfebf005e` | read | 1 | 1 | 1108 | 0x0 |
| `edid` | `0xfebf005f` | read | 1 | 1 | 1109 | 0x32 |
| `edid` | `0xfebf0060` | read | 1 | 1 | 1110 | 0x7d |
| `edid` | `0xfebf0061` | read | 1 | 1 | 1111 | 0x1e |
| `edid` | `0xfebf0062` | read | 1 | 1 | 1112 | 0xa0 |
| `edid` | `0xfebf0063` | read | 1 | 1 | 1113 | 0xff |
| `edid` | `0xfebf0064` | read | 1 | 1 | 1114 | 0x1 |
| `edid` | `0xfebf0065` | read | 1 | 1 | 1115 | 0xa |
| `edid` | `0xfebf0066` | read | 1 | 1 | 1116 | 0x20 |
| `edid` | `0xfebf0067` | read | 1 | 1 | 1117 | 0x20 |
| `edid` | `0xfebf0068` | read | 1 | 1 | 1118 | 0x20 |
| `edid` | `0xfebf0069` | read | 1 | 1 | 1119 | 0x20 |
| `edid` | `0xfebf006a` | read | 1 | 1 | 1120 | 0x20 |
| `edid` | `0xfebf006b` | read | 1 | 1 | 1121 | 0x20 |
| `edid` | `0xfebf006c` | read | 1 | 1 | 1122 | 0x0 |
| `edid` | `0xfebf006d` | read | 1 | 1 | 1123 | 0x0 |
| `edid` | `0xfebf006e` | read | 1 | 1 | 1124 | 0x0 |
| `edid` | `0xfebf006f` | read | 1 | 1 | 1125 | 0xfc |
| `edid` | `0xfebf0070` | read | 1 | 1 | 1126 | 0x0 |
| `edid` | `0xfebf0071` | read | 1 | 1 | 1127 | 0x51 |
| `edid` | `0xfebf0072` | read | 1 | 1 | 1128 | 0x45 |
| `edid` | `0xfebf0073` | read | 1 | 1 | 1129 | 0x4d |
| `edid` | `0xfebf0074` | read | 1 | 1 | 1130 | 0x55 |
| `edid` | `0xfebf0075` | read | 1 | 1 | 1131 | 0x20 |
| `edid` | `0xfebf0076` | read | 1 | 1 | 1132 | 0x4d |
| `edid` | `0xfebf0077` | read | 1 | 1 | 1133 | 0x6f |
| `edid` | `0xfebf0078` | read | 1 | 1 | 1134 | 0x6e |
| `edid` | `0xfebf0079` | read | 1 | 1 | 1135 | 0x69 |
| `edid` | `0xfebf007a` | read | 1 | 1 | 1136 | 0x74 |
| `edid` | `0xfebf007b` | read | 1 | 1 | 1137 | 0x6f |
| `edid` | `0xfebf007c` | read | 1 | 1 | 1138 | 0x72 |
| `edid` | `0xfebf007d` | read | 1 | 1 | 1139 | 0xa |
| `edid` | `0xfebf007e` | read | 1 | 1 | 1140 | 0x1 |
| `edid` | `0xfebf007f` | read | 1 | 1 | 1141 | 0x3a |
| `edid` | `0xfebf0080` | read | 1 | 1 | 1142 | 0x2 |
| `edid` | `0xfebf0081` | read | 1 | 1 | 1143 | 0x3 |
| `edid` | `0xfebf0082` | read | 1 | 1 | 1144 | 0xb |
| `edid` | `0xfebf0083` | read | 1 | 1 | 1145 | 0x0 |
| `edid` | `0xfebf0084` | read | 1 | 1 | 1146 | 0x46 |
| `edid` | `0xfebf0085` | read | 1 | 1 | 1147 | 0x7d |
| `edid` | `0xfebf0086` | read | 1 | 1 | 1148 | 0x65 |
| `edid` | `0xfebf0087` | read | 1 | 1 | 1149 | 0x60 |
| `edid` | `0xfebf0088` | read | 1 | 1 | 1150 | 0x59 |
| `edid` | `0xfebf0089` | read | 1 | 1 | 1151 | 0x1f |
| `edid` | `0xfebf008a` | read | 1 | 1 | 1152 | 0x61 |
| `edid` | `0xfebf008b` | read | 1 | 1 | 1153 | 0x0 |
| `edid` | `0xfebf008c` | read | 1 | 1 | 1154 | 0x0 |
| `edid` | `0xfebf008d` | read | 1 | 1 | 1155 | 0x0 |
| `edid` | `0xfebf008e` | read | 1 | 1 | 1156 | 0x10 |
| `edid` | `0xfebf008f` | read | 1 | 1 | 1157 | 0x0 |
| `edid` | `0xfebf0090` | read | 1 | 1 | 1158 | 0x0 |
| `edid` | `0xfebf0091` | read | 1 | 1 | 1159 | 0x0 |
| `edid` | `0xfebf0092` | read | 1 | 1 | 1160 | 0x0 |
| `edid` | `0xfebf0093` | read | 1 | 1 | 1161 | 0x0 |
| `edid` | `0xfebf0094` | read | 1 | 1 | 1162 | 0x0 |
| `edid` | `0xfebf0095` | read | 1 | 1 | 1163 | 0x0 |
| `edid` | `0xfebf0096` | read | 1 | 1 | 1164 | 0x0 |
| `edid` | `0xfebf0097` | read | 1 | 1 | 1165 | 0x0 |
| `edid` | `0xfebf0098` | read | 1 | 1 | 1166 | 0x0 |
| `edid` | `0xfebf0099` | read | 1 | 1 | 1167 | 0x0 |
| `edid` | `0xfebf009a` | read | 1 | 1 | 1168 | 0x0 |
| `edid` | `0xfebf009b` | read | 1 | 1 | 1169 | 0x0 |
| `edid` | `0xfebf009c` | read | 1 | 1 | 1170 | 0x0 |
| `edid` | `0xfebf009d` | read | 1 | 1 | 1171 | 0x0 |
| `edid` | `0xfebf009e` | read | 1 | 1 | 1172 | 0x0 |
| `edid` | `0xfebf009f` | read | 1 | 1 | 1173 | 0x0 |
| `edid` | `0xfebf00a0` | read | 1 | 1 | 1174 | 0x10 |
| `edid` | `0xfebf00a1` | read | 1 | 1 | 1175 | 0x0 |
| `edid` | `0xfebf00a2` | read | 1 | 1 | 1176 | 0x0 |
| `edid` | `0xfebf00a3` | read | 1 | 1 | 1177 | 0x0 |
| `edid` | `0xfebf00a4` | read | 1 | 1 | 1178 | 0x0 |
| `edid` | `0xfebf00a5` | read | 1 | 1 | 1179 | 0x0 |
| `edid` | `0xfebf00a6` | read | 1 | 1 | 1180 | 0x0 |
| `edid` | `0xfebf00a7` | read | 1 | 1 | 1181 | 0x0 |
| `edid` | `0xfebf00a8` | read | 1 | 1 | 1182 | 0x0 |
| `edid` | `0xfebf00a9` | read | 1 | 1 | 1183 | 0x0 |
| `edid` | `0xfebf00aa` | read | 1 | 1 | 1184 | 0x0 |
| `edid` | `0xfebf00ab` | read | 1 | 1 | 1185 | 0x0 |
| `edid` | `0xfebf00ac` | read | 1 | 1 | 1186 | 0x0 |
| `edid` | `0xfebf00ad` | read | 1 | 1 | 1187 | 0x0 |
| `edid` | `0xfebf00ae` | read | 1 | 1 | 1188 | 0x0 |
| `edid` | `0xfebf00af` | read | 1 | 1 | 1189 | 0x0 |
| `edid` | `0xfebf00b0` | read | 1 | 1 | 1190 | 0x0 |
| `edid` | `0xfebf00b1` | read | 1 | 1 | 1191 | 0x0 |
| `edid` | `0xfebf00b2` | read | 1 | 1 | 1192 | 0x10 |
| `edid` | `0xfebf00b3` | read | 1 | 1 | 1193 | 0x0 |
| `edid` | `0xfebf00b4` | read | 1 | 1 | 1194 | 0x0 |
| `edid` | `0xfebf00b5` | read | 1 | 1 | 1195 | 0x0 |
| `edid` | `0xfebf00b6` | read | 1 | 1 | 1196 | 0x0 |
| `edid` | `0xfebf00b7` | read | 1 | 1 | 1197 | 0x0 |
| `edid` | `0xfebf00b8` | read | 1 | 1 | 1198 | 0x0 |
| `edid` | `0xfebf00b9` | read | 1 | 1 | 1199 | 0x0 |
| `edid` | `0xfebf00ba` | read | 1 | 1 | 1200 | 0x0 |
| `edid` | `0xfebf00bb` | read | 1 | 1 | 1201 | 0x0 |
| `edid` | `0xfebf00bc` | read | 1 | 1 | 1202 | 0x0 |
| `edid` | `0xfebf00bd` | read | 1 | 1 | 1203 | 0x0 |
| `edid` | `0xfebf00be` | read | 1 | 1 | 1204 | 0x0 |
| `edid` | `0xfebf00bf` | read | 1 | 1 | 1205 | 0x0 |
| `edid` | `0xfebf00c0` | read | 1 | 1 | 1206 | 0x0 |
| `edid` | `0xfebf00c1` | read | 1 | 1 | 1207 | 0x0 |
| `edid` | `0xfebf00c2` | read | 1 | 1 | 1208 | 0x0 |
| `edid` | `0xfebf00c3` | read | 1 | 1 | 1209 | 0x0 |
| `edid` | `0xfebf00c4` | read | 1 | 1 | 1210 | 0x10 |
| `edid` | `0xfebf00c5` | read | 1 | 1 | 1211 | 0x0 |
| `edid` | `0xfebf00c6` | read | 1 | 1 | 1212 | 0x0 |
| `edid` | `0xfebf00c7` | read | 1 | 1 | 1213 | 0x0 |
| `edid` | `0xfebf00c8` | read | 1 | 1 | 1214 | 0x0 |
| `edid` | `0xfebf00c9` | read | 1 | 1 | 1215 | 0x0 |
| `edid` | `0xfebf00ca` | read | 1 | 1 | 1216 | 0x0 |
| `edid` | `0xfebf00cb` | read | 1 | 1 | 1217 | 0x0 |
| `edid` | `0xfebf00cc` | read | 1 | 1 | 1218 | 0x0 |
| `edid` | `0xfebf00cd` | read | 1 | 1 | 1219 | 0x0 |
| `edid` | `0xfebf00ce` | read | 1 | 1 | 1220 | 0x0 |
| `edid` | `0xfebf00cf` | read | 1 | 1 | 1221 | 0x0 |
| `edid` | `0xfebf00d0` | read | 1 | 1 | 1222 | 0x0 |
| `edid` | `0xfebf00d1` | read | 1 | 1 | 1223 | 0x0 |
| `edid` | `0xfebf00d2` | read | 1 | 1 | 1224 | 0x0 |
| `edid` | `0xfebf00d3` | read | 1 | 1 | 1225 | 0x0 |
| `edid` | `0xfebf00d4` | read | 1 | 1 | 1226 | 0x0 |
| `edid` | `0xfebf00d5` | read | 1 | 1 | 1227 | 0x0 |
| `edid` | `0xfebf00d6` | read | 1 | 1 | 1228 | 0x10 |
| `edid` | `0xfebf00d7` | read | 1 | 1 | 1229 | 0x0 |
| `edid` | `0xfebf00d8` | read | 1 | 1 | 1230 | 0x0 |
| `edid` | `0xfebf00d9` | read | 1 | 1 | 1231 | 0x0 |
| `edid` | `0xfebf00da` | read | 1 | 1 | 1232 | 0x0 |
| `edid` | `0xfebf00db` | read | 1 | 1 | 1233 | 0x0 |
| `edid` | `0xfebf00dc` | read | 1 | 1 | 1234 | 0x0 |
| `edid` | `0xfebf00dd` | read | 1 | 1 | 1235 | 0x0 |
| `edid` | `0xfebf00de` | read | 1 | 1 | 1236 | 0x0 |
| `edid` | `0xfebf00df` | read | 1 | 1 | 1237 | 0x0 |
| `edid` | `0xfebf00e0` | read | 1 | 1 | 1238 | 0x0 |
| `edid` | `0xfebf00e1` | read | 1 | 1 | 1239 | 0x0 |
| `edid` | `0xfebf00e2` | read | 1 | 1 | 1240 | 0x0 |
| `edid` | `0xfebf00e3` | read | 1 | 1 | 1241 | 0x0 |
| `edid` | `0xfebf00e4` | read | 1 | 1 | 1242 | 0x0 |
| `edid` | `0xfebf00e5` | read | 1 | 1 | 1243 | 0x0 |
| `edid` | `0xfebf00e6` | read | 1 | 1 | 1244 | 0x0 |
| `edid` | `0xfebf00e7` | read | 1 | 1 | 1245 | 0x0 |
| `edid` | `0xfebf00e8` | read | 1 | 1 | 1246 | 0x10 |
| `edid` | `0xfebf00e9` | read | 1 | 1 | 1247 | 0x0 |
| `edid` | `0xfebf00ea` | read | 1 | 1 | 1248 | 0x0 |
| `edid` | `0xfebf00eb` | read | 1 | 1 | 1249 | 0x0 |
| `edid` | `0xfebf00ec` | read | 1 | 1 | 1250 | 0x0 |
| `edid` | `0xfebf00ed` | read | 1 | 1 | 1251 | 0x0 |
| `edid` | `0xfebf00ee` | read | 1 | 1 | 1252 | 0x0 |
| `edid` | `0xfebf00ef` | read | 1 | 1 | 1253 | 0x0 |
| `edid` | `0xfebf00f0` | read | 1 | 1 | 1254 | 0x0 |
| `edid` | `0xfebf00f1` | read | 1 | 1 | 1255 | 0x0 |
| `edid` | `0xfebf00f2` | read | 1 | 1 | 1256 | 0x0 |
| `edid` | `0xfebf00f3` | read | 1 | 1 | 1257 | 0x0 |
| `edid` | `0xfebf00f4` | read | 1 | 1 | 1258 | 0x0 |
| `edid` | `0xfebf00f5` | read | 1 | 1 | 1259 | 0x0 |
| `edid` | `0xfebf00f6` | read | 1 | 1 | 1260 | 0x0 |
| `edid` | `0xfebf00f7` | read | 1 | 1 | 1261 | 0x0 |
| `edid` | `0xfebf00f8` | read | 1 | 1 | 1262 | 0x0 |
| `edid` | `0xfebf00f9` | read | 1 | 1 | 1263 | 0x0 |
| `edid` | `0xfebf00fa` | read | 1 | 1 | 1264 | 0x0 |
| `edid` | `0xfebf00fb` | read | 1 | 1 | 1265 | 0x0 |
| `edid` | `0xfebf00fc` | read | 1 | 1 | 1266 | 0x0 |
| `edid` | `0xfebf00fd` | read | 1 | 1 | 1267 | 0x0 |
| `edid` | `0xfebf00fe` | read | 1 | 1 | 1268 | 0x0 |
| `edid` | `0xfebf00ff` | read | 1 | 1 | 1269 | 0x2f |
| `parallel` | `0x37a` | read | 1 | 1 | 41261 | 0xcc |
| `parallel` | `0x37a` | write | 1 | 1 | 41262 | 0xcc |
| `parallel` | `0x378` | write | 1 | 1 | 41263 | 0xaa |
| `parallel` | `0x378` | read | 1 | 1 | 41264 | 0xaa |
| `io` | `0x27a` | read | 1 | 1 | 41265 | 0xffffffffffffffff |
| `io` | `0x27a` | write | 1 | 1 | 41266 | 0xdf |
| `io` | `0x278` | write | 1 | 1 | 41267 | 0xaa |
| `io` | `0x278` | read | 1 | 1 | 41268 | 0xffffffffffffffff |
| `io` | `0x3e9` | write | 1 | 1 | 41285 | 0x2 |
| `io` | `0x3e9` | read | 1 | 1 | 41286 | 0xffffffffffffffff |
| `io` | `0x2e9` | write | 1 | 1 | 41287 | 0x2 |
| `io` | `0x2e9` | read | 1 | 1 | 41288 | 0xffffffffffffffff |
| `kvmvapic` | `0x7e` | write | 2 | 1 | 54565 | 0x20 |
| `hpet` | `0xfed00000` | read | 8 | 1 | 369152 | 0x9896808086a201 |
| `hpet` | `0xfed00010` | read | 8 | 1 | 369153 | 0x0 |
| `hpet` | `0xfed00010` | write | 8 | 1 | 369154 | 0x1 |
| `hpet` | `0xfed000f0` | read | 8 | 220142 | 369155 | 0x325, 0x46668, 0x46945, 0x46c20, 0x46efb, 0x471d7, 0x474b1, 0x4778b, 0x47a64, 0x47d8b, 0x480e1, 0x48641, 0x48913, 0x48bf8, 0x48ed2, 0x491aa, 0x49483, 0x4975e, 0x49a35, 0x49d0e, 0x49fe8, 0x4a2c0, 0x4a599, 0x4a874, 0x4ab4c, 0x4ae23, 0x4b0fd, 0x4b3d7, 0x4b6b0, 0x4b988, 0x4bc5f, 0x4bf39, 0x4c212, 0x4c4ea, 0x4c7c4, 0x4ca9c, 0x4cd75, 0x4d04a, 0x4d324, 0x4d5fb, 0x4d8d2, 0x4dba8, 0x4de82, 0x4e15b, 0x4e437, 0x4e70f, 0x4e9e7, 0x4ecc1, 0x4f158, 0x4f439, 0x4f70b, 0x4f9e5, 0x4fcbf, 0x4ff9a, 0x50272, 0x5054b, 0x50825, 0x50afc, 0x50dd4, 0x510af, 0x51389, 0x51663, 0x5193c, 0x51c14, 0x51eef, 0x521be, 0x5247e, 0x5273e, 0x52a01, 0x52cc2, 0x52f85, 0x5324c, 0x53509, 0x537cd, 0x53a8d, 0x53d4d, 0x5400e, 0x542cd, 0x54591, 0x54853, 0x54b15, 0x54dd6, 0x55096, 0x5535a, 0x5561d, 0x558df, 0x55ba0, 0x55e60, 0x56122, 0x563e3, 0x566a4, 0x56966, 0x56c2f, 0x56ef1, 0x571b0, 0x57471, 0x57733, 0x579f4, 0x57cb5, 0x57f76, 0x58239, 0x584fc, 0x587be, 0x58a7e, 0x58d3e, 0x59000, 0x592c1, 0x59581, 0x5983f, 0x59b00, 0x59dbf, 0x5a07e, 0x5a33f, 0x5a601, 0x5a8c2, 0x5ab84, 0x5ae47, 0x5b107, 0x5b3c7, 0x5b688, 0x5b949, 0x5bc0a, 0x5beca, 0x5c18c, 0x5c454, 0x5c716, 0x5c9d7, 0x5cc99, 0x5cf5a, 0x5d21c, 0x5d4dc, 0x5d79c, 0x5da5c, 0x5dd1d, 0x5dfdf, 0x5e29d, 0x5e55d, 0x5e820, 0x5eae0, 0x5eda1, 0x5f062, 0x5f324, 0x5f5e3, 0x5f8a5, 0x5fb65, 0x5fe25, 0x600e7, 0x603ec, 0x60737, 0x60c9b, 0x60f5d, 0x6121d, 0x614de, 0x6179e, 0x61a5f, 0x61d1f, 0x61fdf, 0x622a2, 0x62563, 0x62826, 0x62ae7, 0x62da7, 0x63067, 0x6332a, 0x635ec, 0x638ae, 0x63b70, 0x63e31, 0x640f1, 0x643b4, 0x6467b, 0x6493d, 0x64bfc, 0x64ebe, 0x6517f, 0x65441, 0x65743, 0x65a04, 0x65cc5, 0x65f85, 0x66249, 0x66512, 0x667d6, 0x66a96, 0x66d58, 0x6701a, 0x672db, 0x676c4, 0x67994, 0x67c5f, 0x67f23, 0x681e5, 0x684a4, 0x68764, 0x68a27, 0x68ce9, 0x68fb2, 0x69273, 0x69534, 0x697f6, 0x69ab9, 0x69d7a, 0x6a03b, 0x6a2fb, 0x6a5be, 0x6a87e, 0x6ab40, 0x6ae00, 0x6b0c3, 0x6b386, 0x6b648, 0x6b908, 0x6bbc9, 0x6be8b, 0x6c14b, 0x6c40c, 0x6c6cc, 0x6c98e, 0x6cc52, 0x6cf12, 0x6d1d0, 0x6d490, 0x6d754, 0x6da16, 0x6dcd7, 0x6df99, 0x6e25e, 0x6e521, 0x6e7e3, 0x6eaa3, 0x6ed64, 0x6f027, 0x6f2e8, 0x6f5a9, 0x6f868, 0x6fb2b, 0x6fdee, 0x700b0, 0x7036f, 0x70632, 0x708f5, 0x70bb6, 0x70e74, 0x71137, 0x713fa, 0x716bd, 0x7197f, 0x71c40, 0x71f01, 0x721c6, 0x72489, 0x7274a, 0x72a0c, 0x72cce, 0x72f91, 0x73254; truncated after 256 distinct values |
| `acpi-cnt` | `0x604` | write | 2 | 1 | 58768082 | 0x2000 |

## Device events

| Event | Count | First use |
|---|---:|---:|

Normalized argument samples, command opcodes, and truncation flags are in `event-values.json`.
Event counts and register counts can overlap because device-specific and memory-region tracing observe the same access.
Samples are limited to 256 distinct argument strings per event. They do not prove absence of later argument variants.
