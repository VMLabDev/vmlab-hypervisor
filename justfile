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

# Run the D2 benchmark suite (bench/README.md); backend qemu|vmlab-hv
[group('bench')]
bench-run backend="qemu" arch="all" vcpus="1,4" repeats="3" guest="debian-13": (bench-hv-binary backend)
	python3 bench/run.py --backend {{backend}} --arch {{arch}} --vcpus {{vcpus}} --repeats {{repeats}} --guest {{guest}}

# Print the per-workload trend from bench/history
[group('bench')]
bench-report arch="all" backend="all" guest="all":
	python3 bench/report.py --arch {{arch}} --backend {{backend}} --guest {{guest}}

# Fail if the latest run regressed beyond measured noise for its backend
[group('bench')]
bench-check arch="all" backend="all":
	python3 bench/check.py --arch {{arch}} --backend {{backend}}

# Run the bench suite's own unit tests
[group('bench')]
bench-test:
	cd bench && python3 -m unittest

# Build the release binary the vmlab-hv bench backend needs (no-op for qemu)
[private]
bench-hv-binary backend:
	@if [ "{{backend}}" = "vmlab-hv" ]; then cargo build --release -p vmlab-hv; fi
