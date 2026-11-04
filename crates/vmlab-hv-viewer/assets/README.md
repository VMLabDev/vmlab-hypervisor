# Viewer icon

`icon.rgba` is the 64 by 64 RGBA render of vmlab's `guest/logo/icon.svg`.
The viewer embeds these pixels for X11 and XWayland. No image decoder is needed
at runtime. Regenerate from the vmlab checkout:

```sh
guest/logo/render-icon.sh /path/to/vmlab-hypervisor/crates/vmlab-hv-viewer/assets/icon.rgba
```

winit 0.30.13 ignores window icons on Wayland. The viewer sets app ID
`dev.vmlab.viewer`; install `dev.vmlab.viewer.desktop` and the matching hicolor
icons using the release bundle's `install-viewer-icons.sh`.
