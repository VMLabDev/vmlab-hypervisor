set working-directory := '../..'

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
