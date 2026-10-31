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

# Tag of the Sail toolchain container image (toolchain/sail/pins.env pins it)
tcg_image := "vmlab-hv-sail-toolchain:0.20.3"

# Build the pinned Sail toolchain container image from toolchain/sail/pins.env
[private]
tcg-image-build:
	#!/usr/bin/env bash
	set -euo pipefail
	cd toolchain/sail
	args=()
	while IFS='=' read -r key value; do
		case "$key" in ''|'#'*|DEBIAN_BOOKWORM_SLIM_DIGEST) continue ;; esac
		args+=(--build-arg "$key=$value")
	done < pins.env
	docker build --memory 16g --memory-swap 16g -f Containerfile "${args[@]}" -t {{tcg_image}} .

# Emit the textual Jib IR for the pinned sail-riscv model into target/tcg/
[group('tcg')]
tcg-ir-generate: tcg-image-build
	@mkdir -p target/tcg
	docker run --rm --memory 12g --memory-swap 12g --user "$(id -u):$(id -g)" \
		-v "$PWD/target/tcg":/out {{tcg_image}} /opt/scripts/gen-ir.sh
	cd target/tcg && sha256sum -c riscv64.ir.sha256

# Prove the Jib IR generation is deterministic: two runs, identical bytes
[group('tcg')]
tcg-ir-repro-check: tcg-image-build
	#!/usr/bin/env bash
	set -euo pipefail
	tmp=$(mktemp -d)
	trap 'rm -rf "$tmp"' EXIT
	for run in "$tmp/run1" "$tmp/run2"; do
		mkdir -p "$run"
		docker run --rm --memory 12g --memory-swap 12g --user "$(id -u):$(id -g)" \
			-v "$run":/out {{tcg_image}} /opt/scripts/gen-ir.sh
	done
	cmp "$tmp/run1/riscv64.ir" "$tmp/run2/riscv64.ir"
	echo "tcg-ir-repro-check: byte-identical ($(sha256sum < "$tmp/run1/riscv64.ir" | cut -d' ' -f1))"

# Build the Sail C++ model oracle library into target/tcg/oracle/
[group('tcg')]
tcg-oracle-build: tcg-image-build
	@mkdir -p target/tcg/oracle
	docker run --rm --memory 12g --memory-swap 12g --user "$(id -u):$(id -g)" \
		-v "$PWD/target/tcg/oracle":/out {{tcg_image}} /opt/scripts/build-oracle.sh

# Link the oracle library and step a hand-assembled RV64 program through the model
[group('tcg')]
tcg-oracle-test: tcg-oracle-build
	cargo test --manifest-path toolchain/sail/oracle-test/Cargo.toml

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
