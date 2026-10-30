#!/usr/bin/env bash
# Runs inside the toolchain container (see the tcg recipes): build the
# Sail RISC-V C++ model plus our oracle wrapper into one static library
# in /out, with the headers and the model configuration the harness needs.
#
# Layout produced:
#   /out/liboracle.a                  — linkable whole-model archive
#   /out/oracle.h                     — the C ABI (see toolchain/sail/README.md)
#   /out/riscv64-config.json          — the model config the model was built for
#   /out/oracle.versions.txt          — versions and hashes
#
# Link with -lgmp (the Sail runtime's arbitrary-precision ints).
set -euo pipefail

OUT=/out
SRC=${SAIL_RISCV:-/opt/sail-riscv}
BUILD=${SAIL_RISCV_BUILD:-/work/build}
CFG_NAME=${RISCV_CONFIG:-rv64d_v128_e64}
CFG="$BUILD/config/$CFG_NAME.json"

# The image builds this once; rebuild (cheap, cached) if missing.
cmake --build "$BUILD" --target riscv_model --parallel "$(nproc)" > /dev/null

SAIL_LIB="$(sail --dir)/lib"
SAIL_VERSION_FULL=$(sail --version)
SAIL_VERSION_PINNED=${SAIL_VERSION:?image must set SAIL_VERSION}
MODEL_COMMIT=${SAIL_RISCV_COMMIT:?image must set SAIL_RISCV_COMMIT}
ISLA_COMMIT_PINNED=${ISLA_COMMIT:?image must set ISLA_COMMIT}
OBJDIR=$(mktemp -d)
trap 'rm -rf "$OBJDIR"' EXIT

# The oracle wrapper: the model header, the platform interface it derives
# from, and the Sail runtime headers.
g++ -std=c++17 -O2 -fPIC -c /opt/oracle/oracle.cpp -o "$OBJDIR/oracle.o" \
    -I/opt/oracle \
    -I"$BUILD" \
    -I"$SRC/c_emulator" \
    -I"$SAIL_LIB" \
    -DSAIL_ORACLE_SAIL_VERSION="\"$SAIL_VERSION_PINNED\"" \
    -DSAIL_ORACLE_MODEL_COMMIT="\"$MODEL_COMMIT\"" \
    -Wno-unused-parameter

# Merge every archive cmake built for the model into one linkable whole,
# prefixing object names so same-named objects from different libraries
# cannot collide in the archive.
for A in $(find "$BUILD" -name '*.a'); do
    NAME=$(basename "$A" .a)
    NAME=${NAME#lib}
    D="$OBJDIR/x_$NAME"
    mkdir -p "$D"
    (cd "$D" && ar x "$A")
    for O in "$D"/*.o; do
        [ -e "$O" ] || continue
        mv "$O" "$OBJDIR/${NAME}_$(basename "$O")"
    done
done

ar rcs "$OUT/liboracle.a" "$OBJDIR"/*.o
ranlib "$OUT/liboracle.a"

cp /opt/oracle/oracle.h "$OUT/oracle.h"
cp "$CFG" "$OUT/riscv64-config.json"

{
    echo "sail $SAIL_VERSION_FULL"
    echo "isla-sail $ISLA_COMMIT_PINNED"
    echo "sail-riscv $MODEL_COMMIT"
    echo "config $CFG_NAME $(sha256sum "$CFG" | cut -d' ' -f1)"
    echo "liboracle_sha256 $(sha256sum "$OUT/liboracle.a" | cut -d' ' -f1)"
} > "$OUT/oracle.versions.txt"

chmod 644 "$OUT"/*

echo "oracle-build: wrote $OUT/liboracle.a ($(du -h "$OUT/liboracle.a" | cut -f1))"
