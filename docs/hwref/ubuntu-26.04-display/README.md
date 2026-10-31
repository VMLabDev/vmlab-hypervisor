# Ubuntu 26.04 display reference

This display-only hwref capture contains the DRM connector EDID and fbdev
size read through vmlab-agent in the Ubuntu 26.04.3 template under QEMU 11.1.2.
The default virtio-vga EDID has 256 bytes and prefers 1280x800. Both EDID
blocks have valid checksums. `edid-decode --check` reports a missing CTA
VCDB in QEMU's extension. D30 requires preserving those bytes.

`qemu_ubuntu_display_reference` records the QEMU argv in its test source.
It uses q35, KVM, two host vCPUs, 4096 MiB, virtio-serial, virtio-blk and
default virtio-vga, matching vmlab's display defaults. Each run boots a
disposable overlay. No QEMU source was read.

`qemu_ubuntu_standard_vga_display_reference` checks the same monitor through
standard VGA. Its EDID is exposed through BAR2 as the standard VGA MMIO
specification requires: https://www.qemu.org/docs/master/specs/standard-vga.html.

Regenerate the EDID and verify the framebuffer size:

```sh
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  env VMLAB_HV_EDID_CAPTURE="$PWD/docs/hwref/ubuntu-26.04-display/guest/drm/Virtual-1.edid" \
  cargo test -p vmlab-hv qemu_ubuntu_display_reference -- --nocapture
```

Build `vmlab-hv`, then capture the same values through S1:

```sh
systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0 -- \
  env VMLAB_HV_DISPLAY_HWREF="$PWD/.scratch/display/hwref-hv" \
  cargo test -p vmlab-hv ubuntu_26_04_native_display_input_and_rfb -- --nocapture
python3 scripts/hwref/capture.py diff docs/hwref/ubuntu-26.04-display .scratch/display/hwref-hv
```

`display.json` identifies the limited capture scope. The diff requires both
display artifacts and refuses to compare a display capture with a full
hardware capture. It compares all EDID bytes without normalization.
