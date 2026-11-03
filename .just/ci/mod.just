set working-directory := '../..'

virgl_version := '1.3.0'
virgl_sha256 := '065bc56e89e6f631f96101cd62eba0748e48eb888b434edc86e89d05395e76f3'
virgl_prefix := env('VIRGL_PREFIX', justfile_directory() / '.ci-cache/virgl')

[default, private]
main:
	@just --list ci

# Everything a change must pass before it can merge
[group('ci')]
check: rust-fmt rust-lint rust-test

# Check formatting
[group('ci')]
rust-fmt:
	cargo fmt --all --check

# Lint the workspace
[group('ci')]
rust-lint:
	cargo clippy --workspace --all-targets -- -D warnings

# Run the test suite
[group('ci')]
rust-test:
	cargo test --workspace

# Install Ubuntu build dependencies and tools for tests that need no guest images
[group('ci')]
native-deps-install:
	sudo apt-get update
	sudo apt-get install -y --no-install-recommends build-essential gcc-multilib liblzma-dev curl pkg-config meson ninja-build python3-yaml libepoxy-dev libdrm-dev libgbm-dev libegl-dev libvulkan-dev glslang-tools libpipewire-0.3-dev libpulse-dev clang libclang-dev qemu-utils nasm acpica-tools imagemagick cpio xvfb libvulkan1 mesa-vulkan-drivers seabios xorriso edid-decode pipewire-bin
	sudo apt-get install -y --no-install-recommends libxkbcommon-x11-0 libxkbcommon0 libwayland-client0 libx11-6 libxcursor1 libxrandr2 libxi6 libxfixes3 libxtst6
	sudo install -d /usr/share/qemu
	test -e /usr/share/qemu/vgabios-stdvga.bin || sudo ln -s /usr/share/seabios/vgabios-stdvga.bin /usr/share/qemu/vgabios-stdvga.bin
	test -e /usr/share/qemu/vgabios-virtio.bin || sudo ln -s /usr/share/seabios/vgabios-virtio.bin /usr/share/qemu/vgabios-virtio.bin
	command -v magick || sudo ln -s /usr/bin/convert /usr/local/bin/magick

# Emit the virglrenderer cache identity for GitHub Actions
[group('ci')]
virgl-cache-key:
	@echo 'key=virglrenderer-{{virgl_version}}-{{virgl_sha256}}'

# Build the pinned virglrenderer with Venus and its default render server
[group('ci')]
virgl-build: virgl-fetch
	meson setup target/ci/virgl-build target/ci/virglrenderer-{{virgl_version}} --prefix='{{virgl_prefix}}' --libdir=lib --buildtype=release -Dvenus=true -Dtests=false
	ninja -C target/ci/virgl-build install
	pkg-config --with-path='{{virgl_prefix}}/lib/pkgconfig' --atleast-version={{virgl_version}} virglrenderer
	test -x '{{virgl_prefix}}/libexec/virgl_render_server'

[private]
virgl-fetch:
	mkdir -p target/ci
	curl -fsSL -o target/ci/virgl.tar.gz https://gitlab.freedesktop.org/virgl/virglrenderer/-/archive/{{virgl_version}}/virglrenderer-{{virgl_version}}.tar.gz
	echo '{{virgl_sha256}}  target/ci/virgl.tar.gz' | sha256sum -c -
	tar -C target/ci -xzf target/ci/virgl.tar.gz
