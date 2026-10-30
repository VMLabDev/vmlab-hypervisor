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
