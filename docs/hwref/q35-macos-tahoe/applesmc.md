# Apple SMC commands and key names

Ports are 0x300 data, 0x304 command/status, and 0x31e error.
All SMC values were discarded by the trace consumer before persistence.
Only commands, four-character key names, and access counts are retained.

| Command | Name | Writes |
|---|---|---:|
| 0x10 | READ_KEY | 7 |
| 0x11 | WRITE_KEY | 8 |
| 0x13 | GET_KEY_TYPE | 64 |

| Command | Key name | Count |
|---|---|---:|
| 0x10 | BEMB | 1 |
| 0x10 | MSSD | 1 |
| 0x10 | MSSW | 1 |
| 0x10 | NATJ | 1 |
| 0x10 | OSK0 | 1 |
| 0x10 | OSK1 | 1 |
| 0x10 | OSWD | 1 |

No four-byte key payload followed the observed 0x11 or 0x13 command
writes. Their requested key names and success are therefore not established.
The OSK0 and OSK1 names are protocol identifiers. Their values are omitted.
AppleSMC bound in the guest; see `guest-static.json`.
