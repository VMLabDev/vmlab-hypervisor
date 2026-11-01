#!/usr/bin/env bash
# Emit the published Armv9.4-A model using the pinned Sail/isla toolchain.
set -euo pipefail

SRC=${SAIL_ARM:-/opt/sail-arm/arm-v9.4-a}
OUT=/out
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
ln -s "$SRC/src" "$WORK/src"
cd "$WORK"

# Use the upstream source order and Isla splice. All caches and outputs
# belong to this run; the pinned model checkout stays read-only.
if ! make -f "$SRC/Makefile" gen_ir > isla-sail.log 2>&1; then
    tail -c 8192 isla-sail.log >&2
    exit 1
fi
test -s ir/armv9.ir
cp ir/armv9.ir "$OUT/aarch64.ir"
IR_SHA=$(sha256sum ir/armv9.ir | cut -d' ' -f1)
{
    echo "sail $(sail --version)"
    echo "isla-sail ${ISLA_COMMIT:?image must set ISLA_COMMIT}"
    echo "sail-arm ${SAIL_ARM_COMMIT:?image must set SAIL_ARM_COMMIT} arm-v9.4-a"
    echo "command make -f $SRC/Makefile gen_ir"
    echo "output_sha256 $IR_SHA"
    echo "output_bytes $(wc -c < ir/armv9.ir)"
    echo "output_lines $(wc -l < ir/armv9.ir)"
} > "$OUT/aarch64.ir.versions.txt"
echo "$IR_SHA  aarch64.ir" > "$OUT/aarch64.ir.sha256"
tail -c 8192 isla-sail.log > "$OUT/aarch64.ir.log"
chmod 644 "$OUT"/aarch64.ir*
echo "gen-ir-arm: wrote $OUT/aarch64.ir ($(wc -c < ir/armv9.ir) bytes)"
