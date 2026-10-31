# Transitional virtio registers

PCI 00:01.0 is virtio-serial 1af4:1003; PCI 00:02.0 is virtio-net
1af4:1000. Both advertise legacy I/O BAR0 and modern capability BAR4.
Guest agent commands and file readback passed through virtio-serial;
two ping replies from 10.219.89.1 prove the virtio-net path.
Legacy `virtio-pci` callback offsets are relative to each device BAR0;
that shared trace region does not identify which device made an access.
Modern regions carry device names. Presence of both types of access
does not by itself establish the final negotiated transport for every queue.
See `event-values.json` for status transitions and serial control events.

| Region | Offset or port | R/W | Size | Count | First event | Values |
|---|---|---|---|---:|---:|---|
| virtio-pci | 0x6040 | read | 4 | 1 | 31442 | 0x79000006 |
| virtio-pci | 0x6044 | write | 4 | 1 | 31450 | 0x2 |
| virtio-pci | 0x6048 | write | 4 | 4 | 31446 | 0x7e808, 0x7e80e, 0x7e811, 0x7e814 |
| virtio-pci | 0x604c | read | 2 | 4 | 31445 | 0x20, 0x80 |
| virtio-pci | 0x604e | write | 2 | 4 | 31444 | 0x2, 0x3, 0x4, 0x5 |
| virtio-pci | 0x6052 | write | 1 | 5 | 31431 | 0x0, 0x1, 0x3, 0x7 |
| virtio-pci | 0x6058 | read | 4 | 1 | 31443 | 0x1f |
| virtio-pci-common-virtio-net | 0x81004000 | write | 4 | 2 | 293252 | 0x0, 0x1 |
| virtio-pci-common-virtio-net | 0x81004004 | read | 4 | 2 | 293253 | 0x101, 0x30bf8024 |
| virtio-pci-common-virtio-net | 0x81004008 | write | 4 | 2 | 293256 | 0x0, 0x1 |
| virtio-pci-common-virtio-net | 0x8100400c | write | 4 | 2 | 293257 | 0x1, 0x30a70020 |
| virtio-pci-common-virtio-net | 0x81004010 | read | 2 | 1 | 293593 | 0x3 |
| virtio-pci-common-virtio-net | 0x81004010 | write | 2 | 1 | 293589 | 0x3 |
| virtio-pci-common-virtio-net | 0x81004014 | read | 1 | 11 | 289001 | 0x0, 0x1, 0x3, 0xb, 0xf |
| virtio-pci-common-virtio-net | 0x81004014 | write | 1 | 6 | 288994 | 0x0, 0x1, 0x3, 0xb, 0xf |
| virtio-pci-common-virtio-net | 0x81004016 | write | 2 | 14 | 293331 | 0x0, 0x1, 0x2 |
| virtio-pci-common-virtio-net | 0x81004018 | read | 2 | 3 | 293337 | 0x100, 0x40 |
| virtio-pci-common-virtio-net | 0x8100401a | read | 2 | 3 | 293382 | 0x0, 0x1, 0x2 |
| virtio-pci-common-virtio-net | 0x8100401a | write | 2 | 3 | 293380 | 0x0, 0x1, 0x2 |
| virtio-pci-common-virtio-net | 0x8100401c | write | 2 | 3 | 293419 | 0x1 |
| virtio-pci-common-virtio-net | 0x8100401e | read | 2 | 2 | 1416077 | 0x1, 0x2 |
| virtio-pci-common-virtio-net | 0x81004020 | write | 4 | 3 | 293387 | 0x1a9bd000, 0x1a9c0000, 0x1a9c7000 |
| virtio-pci-common-virtio-net | 0x81004024 | write | 4 | 3 | 293391 | 0x0 |
| virtio-pci-common-virtio-net | 0x81004028 | write | 4 | 3 | 293395 | 0x1a9be000, 0x1a9c1000, 0x1a9c7400 |
| virtio-pci-common-virtio-net | 0x8100402c | write | 4 | 3 | 293400 | 0x0 |
| virtio-pci-common-virtio-net | 0x81004030 | write | 4 | 3 | 293404 | 0x1a9bf000, 0x1a9c2000, 0x1a9c8000 |
| virtio-pci-common-virtio-net | 0x81004034 | write | 4 | 3 | 293409 | 0x0 |
| virtio-pci-common-virtio-net | 0x800004000 | write | 4 | 4 | 109312 | 0x0, 0x1 |
| virtio-pci-common-virtio-net | 0x800004004 | read | 2 | 4 | 109313 | 0x101, 0x30bf8024 |
| virtio-pci-common-virtio-net | 0x800004008 | write | 4 | 2 | 109329 | 0x0, 0x1 |
| virtio-pci-common-virtio-net | 0x80000400c | write | 4 | 2 | 109330 | 0x1, 0x20 |
| virtio-pci-common-virtio-net | 0x800004012 | read | 2 | 1 | 109337 | 0x3 |
| virtio-pci-common-virtio-net | 0x800004014 | read | 1 | 7 | 109305 | 0x0, 0x1, 0x3, 0xb |
| virtio-pci-common-virtio-net | 0x800004014 | write | 1 | 6 | 109302 | 0x0, 0x1, 0x3, 0xb, 0xf |
| virtio-pci-common-virtio-net | 0x800004016 | write | 2 | 4 | 109339 | 0x0, 0x1 |
| virtio-pci-common-virtio-net | 0x800004018 | read | 2 | 2 | 109340 | 0x100 |
| virtio-pci-common-virtio-net | 0x800004018 | write | 2 | 2 | 109342 | 0x100 |
| virtio-pci-common-virtio-net | 0x80000401c | write | 2 | 2 | 109384 | 0x1 |
| virtio-pci-common-virtio-net | 0x80000401e | read | 2 | 2 | 109341 | 0x0, 0x1 |
| virtio-pci-common-virtio-net | 0x800004020 | write | 4 | 2 | 109343 | 0x7e61a000, 0x7e61e000 |
| virtio-pci-common-virtio-net | 0x800004024 | write | 4 | 2 | 109344 | 0x0 |
| virtio-pci-common-virtio-net | 0x800004028 | write | 4 | 2 | 109345 | 0x7e61b000, 0x7e61f000 |
| virtio-pci-common-virtio-net | 0x80000402c | write | 4 | 2 | 109346 | 0x0 |
| virtio-pci-common-virtio-net | 0x800004030 | write | 4 | 2 | 109347 | 0x7e61c000, 0x7e620000 |
| virtio-pci-common-virtio-net | 0x800004034 | write | 4 | 2 | 109348 | 0x0 |
| virtio-pci-common-virtio-serial | 0x81000000 | write | 4 | 6 | 289302 | 0x0, 0x1 |
| virtio-pci-common-virtio-serial | 0x81000004 | read | 4 | 6 | 289305 | 0x101, 0x30000006 |
| virtio-pci-common-virtio-serial | 0x81000008 | write | 4 | 2 | 289314 | 0x0, 0x1 |
| virtio-pci-common-virtio-serial | 0x8100000c | write | 4 | 2 | 289317 | 0x1, 0x30000002 |
| virtio-pci-common-virtio-serial | 0x81000014 | read | 1 | 11 | 288986 | 0x0, 0x1, 0x3, 0xb, 0xf |
| virtio-pci-common-virtio-serial | 0x81000014 | write | 1 | 6 | 288982 | 0x0, 0x1, 0x3, 0xb, 0xf |
| virtio-pci-common-virtio-serial | 0x81000016 | write | 2 | 260 | 289397 | 0x0, 0x1, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x2, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x3, 0x30, 0x31, 0x32, 0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x4, 0x5, 0x6, 0x7, 0x8, 0x9, 0xa, 0xb, 0xc, 0xd, 0xe, 0xf |
| virtio-pci-common-virtio-serial | 0x81000018 | read | 2 | 64 | 289400 | 0x20, 0x80 |
| virtio-pci-common-virtio-serial | 0x8100001a | read | 2 | 64 | 289429 | 0x0, 0x1 |
| virtio-pci-common-virtio-serial | 0x8100001a | write | 2 | 64 | 289426 | 0x0, 0x1 |
| virtio-pci-common-virtio-serial | 0x8100001c | write | 2 | 64 | 289596 | 0x1 |
| virtio-pci-common-virtio-serial | 0x8100001e | read | 2 | 4 | 293838 | 0x2, 0x3, 0x4, 0x5 |
| virtio-pci-common-virtio-serial | 0x81000020 | write | 4 | 64 | 289575 | 0x1a87d000, 0x1a87f000, 0x1a881000, 0x1a883000, 0x1a885000, 0x1a887000, 0x1a889000, 0x1a88b000, 0x1a88d000, 0x1a88f000, 0x1a891000, 0x1a893000, 0x1a895000, 0x1a897000, 0x1a899000, 0x1a89b000, 0x1a89d000, 0x1a89f000, 0x1a8a1000, 0x1a8a3000, 0x1a8a5000, 0x1a8a7000, 0x1a8a9000, 0x1a8ab000, 0x1a8ed000, 0x1a8f8000, 0x1a904000, 0x1a910000, 0x1a91c000, 0x1a91e000, 0x1a941000, 0x1a943000, 0x1a966000, 0x1a968000, 0x1a98d000, 0x1a98f000, 0x1a991000, 0x1a993000, 0x1a995000, 0x1a997000, 0x1a999000, 0x1a99b000, 0x1a99d000, 0x1a99f000, 0x1a9a1000, 0x1a9a3000, 0x1a9a5000, 0x1a9a7000, 0x1a9a9000, 0x1a9ab000, 0x1a9ad000, 0x1a9af000, 0x1a9b1000, 0x1a9b3000, 0x1a9b5000, 0x1a9b7000, 0x1a9b9000, 0x1a9bb000, 0x1a9c3000, 0x1a9c5000, 0x1a9c9000, 0x1a9cb000, 0x1a9cd000, 0x1a9cf000 |
| virtio-pci-common-virtio-serial | 0x81000024 | write | 4 | 64 | 289578 | 0x0 |
| virtio-pci-common-virtio-serial | 0x81000028 | write | 4 | 64 | 289581 | 0x1a87d200, 0x1a87f200, 0x1a881800, 0x1a883800, 0x1a885800, 0x1a887800, 0x1a889800, 0x1a88b800, 0x1a88d800, 0x1a88f800, 0x1a891800, 0x1a893800, 0x1a895800, 0x1a897800, 0x1a899800, 0x1a89b800, 0x1a89d800, 0x1a89f800, 0x1a8a1800, 0x1a8a3800, 0x1a8a5800, 0x1a8a7800, 0x1a8a9800, 0x1a8ab800, 0x1a8ed800, 0x1a8f8800, 0x1a904800, 0x1a910800, 0x1a91c800, 0x1a91e800, 0x1a941800, 0x1a943800, 0x1a966800, 0x1a968800, 0x1a98d800, 0x1a98f800, 0x1a991800, 0x1a993800, 0x1a995800, 0x1a997800, 0x1a999800, 0x1a99b800, 0x1a99d800, 0x1a99f800, 0x1a9a1800, 0x1a9a3800, 0x1a9a5800, 0x1a9a7800, 0x1a9a9800, 0x1a9ab800, 0x1a9ad800, 0x1a9af800, 0x1a9b1800, 0x1a9b3800, 0x1a9b5800, 0x1a9b7800, 0x1a9b9800, 0x1a9bb800, 0x1a9c3800, 0x1a9c5800, 0x1a9c9800, 0x1a9cb800, 0x1a9cd800, 0x1a9cf800 |
| virtio-pci-common-virtio-serial | 0x8100002c | write | 4 | 64 | 289584 | 0x0 |
| virtio-pci-common-virtio-serial | 0x81000030 | write | 4 | 64 | 289587 | 0x1a87e000, 0x1a880000, 0x1a882000, 0x1a884000, 0x1a886000, 0x1a888000, 0x1a88a000, 0x1a88c000, 0x1a88e000, 0x1a890000, 0x1a892000, 0x1a894000, 0x1a896000, 0x1a898000, 0x1a89a000, 0x1a89c000, 0x1a89e000, 0x1a8a0000, 0x1a8a2000, 0x1a8a4000, 0x1a8a6000, 0x1a8a8000, 0x1a8aa000, 0x1a8ac000, 0x1a8ee000, 0x1a8f9000, 0x1a905000, 0x1a911000, 0x1a91d000, 0x1a91f000, 0x1a942000, 0x1a944000, 0x1a967000, 0x1a969000, 0x1a98e000, 0x1a990000, 0x1a992000, 0x1a994000, 0x1a996000, 0x1a998000, 0x1a99a000, 0x1a99c000, 0x1a99e000, 0x1a9a0000, 0x1a9a2000, 0x1a9a4000, 0x1a9a6000, 0x1a9a8000, 0x1a9aa000, 0x1a9ac000, 0x1a9ae000, 0x1a9b0000, 0x1a9b2000, 0x1a9b4000, 0x1a9b6000, 0x1a9b8000, 0x1a9ba000, 0x1a9bc000, 0x1a9c4000, 0x1a9c6000, 0x1a9ca000, 0x1a9cc000, 0x1a9ce000, 0x1a9d0000 |
| virtio-pci-common-virtio-serial | 0x81000034 | write | 4 | 64 | 289590 | 0x0 |
| virtio-pci-device-virtio-net | 0x81006000 | read | 1 | 1 | 293260 | 0x52 |
| virtio-pci-device-virtio-net | 0x81006001 | read | 1 | 1 | 293272 | 0x54 |
| virtio-pci-device-virtio-net | 0x81006002 | read | 1 | 1 | 293274 | 0x0 |
| virtio-pci-device-virtio-net | 0x81006003 | read | 1 | 1 | 293276 | 0xf8 |
| virtio-pci-device-virtio-net | 0x81006004 | read | 1 | 1 | 293279 | 0xd6 |
| virtio-pci-device-virtio-net | 0x81006005 | read | 1 | 1 | 293280 | 0xa9 |
| virtio-pci-device-virtio-net | 0x81006006 | read | 1 | 1 | 293282 | 0x1 |
| virtio-pci-device-virtio-net | 0x81006007 | read | 1 | 1 | 293284 | 0x0 |
| virtio-pci-device-virtio-net | 0x81006008 | read | 1 | 1 | 293288 | 0xffffffff |
| virtio-pci-device-virtio-net | 0x81006009 | read | 1 | 1 | 293294 | 0xffffffff |
| virtio-pci-device-virtio-net | 0x8100600a | read | 1 | 1 | 293299 | 0xffffffff |
| virtio-pci-device-virtio-net | 0x8100600b | read | 1 | 1 | 293306 | 0xffffffff |
| virtio-pci-device-virtio-net | 0x8100600c | read | 1 | 1 | 293317 | 0xffffffff |
| virtio-pci-device-virtio-net | 0x8100600d | read | 1 | 1 | 293319 | 0xffffffff |
| virtio-pci-device-virtio-net | 0x8100600e | read | 1 | 1 | 293320 | 0xffffffff |
| virtio-pci-device-virtio-net | 0x8100600f | read | 1 | 1 | 293322 | 0xffffffff |
| virtio-pci-device-virtio-net | 0x81006010 | read | 1 | 1 | 293324 | 0xffffffff |
| virtio-pci-device-virtio-net | 0x800006000 | read | 1 | 1 | 109316 | 0x52 |
| virtio-pci-device-virtio-net | 0x800006001 | read | 1 | 1 | 109317 | 0x54 |
| virtio-pci-device-virtio-net | 0x800006002 | read | 1 | 1 | 109318 | 0x0 |
| virtio-pci-device-virtio-net | 0x800006003 | read | 1 | 1 | 109319 | 0xf8 |
| virtio-pci-device-virtio-net | 0x800006004 | read | 1 | 1 | 109320 | 0xd6 |
| virtio-pci-device-virtio-net | 0x800006005 | read | 1 | 1 | 109321 | 0xa9 |
| virtio-pci-device-virtio-serial | 0x81002000 | read | 1 | 1 | 289361 | 0x0 |
| virtio-pci-device-virtio-serial | 0x81002001 | read | 1 | 1 | 289364 | 0x0 |
| virtio-pci-device-virtio-serial | 0x81002002 | read | 1 | 1 | 289367 | 0x0 |
| virtio-pci-device-virtio-serial | 0x81002003 | read | 1 | 1 | 289370 | 0x0 |
| virtio-pci-device-virtio-serial | 0x81002004 | read | 1 | 1 | 289373 | 0x1f |
| virtio-pci-device-virtio-serial | 0x81002005 | read | 1 | 1 | 289376 | 0x0 |
| virtio-pci-device-virtio-serial | 0x81002006 | read | 1 | 1 | 289379 | 0x0 |
| virtio-pci-device-virtio-serial | 0x81002007 | read | 1 | 1 | 289382 | 0x0 |
| virtio-pci-device-virtio-serial | 0x81002008 | read | 1 | 1 | 289385 | 0x0 |
| virtio-pci-device-virtio-serial | 0x81002009 | read | 1 | 1 | 289388 | 0x0 |
| virtio-pci-device-virtio-serial | 0x8100200a | read | 1 | 1 | 289391 | 0x0 |
| virtio-pci-device-virtio-serial | 0x8100200b | read | 1 | 1 | 289394 | 0x0 |
| virtio-pci-isr-virtio-net | 0x800005000 | read | 1 | 627 | 109390 | 0x0 |
