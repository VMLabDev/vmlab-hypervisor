#!/usr/bin/env bash
# Runs inside the toolchain container (see the tcg recipes): emit the
# textual Jib IR for the pinned sail-riscv model into /out, with tool
# versions and the output sha256 recorded beside it.
#
# The flags are the ones research notes/01 measured for current
# sail-riscv master; encdec_forwards is preserved so our emitter can
# dispatch on decode, matching the Pydrofoil AOT shape.
set -euo pipefail

OUT=/out
SRC=${SAIL_RISCV:-/opt/sail-riscv}
BUILD=${SAIL_RISCV_BUILD:-/work/build}
CFG_NAME=${RISCV_CONFIG:-rv64d_v128_e64}
CFG="$BUILD/config/$CFG_NAME.json"

test -f "$CFG" || { echo "gen-ir: missing config $CFG (container image broken?)" >&2; exit 1; }

# Fresh scratch dir per run: the z3 memo cache must never leak between
# runs, or reproducibility is only apparent.
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# Reproduce the research invocation shape (run from a directory holding
# `model/`) without writing z3 caches into the pinned checkout.
ln -s "$SRC/model" "$WORK/model"
cd "$WORK"

isla-sail -dno_cast -O -Oconstant_fold -memo_z3 -c_no_main \
    --isla-preserve encdec_forwards \
    --project model/riscv.sail_project --all-modules \
    --config "$CFG" \
    -o riscv64 2> isla-sail.log

test -s riscv64.ir || { echo "gen-ir: isla-sail produced no output" >&2; cat isla-sail.log >&2; exit 1; }

IR_SHA=$(sha256sum riscv64.ir | cut -d' ' -f1)
CFG_SHA=$(sha256sum "$CFG" | cut -d' ' -f1)

cp riscv64.ir "$OUT/riscv64.ir"
tail -n 50 isla-sail.log > "$OUT/riscv64.ir.log" || true
{
    echo "sail $(sail --version)"
    echo "isla-sail ${ISLA_COMMIT:?image must set ISLA_COMMIT}"
    echo "sail-riscv ${SAIL_RISCV_COMMIT:?image must set SAIL_RISCV_COMMIT}"
    echo "config $CFG_NAME $CFG_SHA"
    echo "command isla-sail -dno_cast -O -Oconstant_fold -memo_z3 -c_no_main --isla-preserve encdec_forwards --project model/riscv.sail_project --all-modules --config $CFG -o riscv64"
    echo "output_sha256 $IR_SHA"
    echo "output_bytes $(wc -c < riscv64.ir)"
    echo "output_lines $(wc -l < riscv64.ir)"
} > "$OUT/riscv64.ir.versions.txt"
echo "$IR_SHA  riscv64.ir" > "$OUT/riscv64.ir.sha256"
chmod 644 "$OUT"/riscv64.ir*

echo "gen-ir: wrote $OUT/riscv64.ir ($(wc -c < riscv64.ir) bytes)"
