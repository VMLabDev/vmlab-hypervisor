# vmlab-hypervisor task runner

# Verification gate
mod ci '.just/ci'

[default, private]
main:
	@just --list

# Build the workspace
[group('dev')]
workspace-build:
	cargo build --workspace

# Run the workspace tests
[group('dev')]
workspace-test:
	cargo test --workspace

# Build the pinned legacy and EFI virtio-net option ROMs
[group('dev')]
firmware-ipxe-build:
	uv run python firmware/ipxe/build.py

# Pin the Alpine minirootfs the S1 rootfs builds from
alpine_url := "https://dl-cdn.alpinelinux.org/alpine/v3.22/releases/x86_64/alpine-minirootfs-3.22.1-x86_64.tar.gz"
alpine_sha := "0e5cc5702ad72a4e151f219976ba946d50161c3acce210ef3b122a529aba1270"

# Fetch (once, cached under target/) and verify the pinned Alpine minirootfs
[group('test')]
s1-tarball:
	@mkdir -p target/s1
	@test -f target/s1/alpine.tar.gz || curl -fsSL -o target/s1/alpine.tar.gz {{alpine_url}}
	@echo "{{alpine_sha}}  target/s1/alpine.tar.gz" | sha256sum -c -

# Build the reproducible squashfs rootfs the S1 tests boot (needs sqfstar)
[group('test')]
s1-rootfs-build: s1-tarball
	@command -v sqfstar > /dev/null || (echo 'sqfstar is required to build the S1 rootfs' && exit 1)
	@gzip -dc target/s1/alpine.tar.gz | sqfstar -no-xattrs -all-root -repro-time 0 -comp gzip -force target/s1/rootfs.sqfs

# Run the S1 integration suite against the built rootfs
[group('test')]
s1-tests-run: s1-rootfs-build
	cargo test -p vmlab-hv-tests -- --nocapture
