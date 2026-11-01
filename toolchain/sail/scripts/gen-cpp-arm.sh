#!/usr/bin/env bash
# Generate an independent C++ model for the in-process AArch64 oracle.
set -euo pipefail
SRC=${SAIL_ARM:-/opt/sail-arm/arm-v9.4-a}
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
ln -s "$SRC/src" "$WORK/src"
cd "$WORK"
mapfile -t sources < <(make -s -f "$SRC/Makefile" -f - print-sources <<'MAKE'
print-sources:
	@printf '%s\n' $(SAIL_SRCS) $(SAIL_MAIN)
MAKE
)
if ! sail --cpp --cpp-namespace arm_oracle --c-no-main --c-generate-header \
    --c-preserve __InitSystem --c-preserve __InstructionExecute --c-preserve __CycleEnd \
    -O -Oconstant_fold "${sources[@]}" -o /out/aarch64 > /out/aarch64.cpp.log 2>&1; then
    tail -c 8192 /out/aarch64.cpp.log >&2
    exit 1
fi
test -s /out/aarch64.cpp
test -s /out/aarch64.h
echo "gen-cpp-arm: wrote independent C++ oracle sources"
