#!/usr/bin/env python3
"""Run the complete 131-program I/M corpus plus the two FP decoder regressions."""
import argparse
import json
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=Path("target/tcg/corpus/manifest.json"))
    parser.add_argument("--binary", type=Path, default=Path("target/release/examples/lockstep"))
    parser.add_argument("--fp-dir", type=Path, default=Path("target/tcg/fp"))
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--part", type=int, default=0)
    parser.add_argument("--parts", type=int, default=1)
    args = parser.parse_args()
    if args.parts < 1 or not 0 <= args.part < args.parts:
        parser.error("part must be in 0..parts and parts must be positive")
    manifest = json.loads(args.manifest.read_text())
    if len(manifest) != 131:
        parser.error("expected all 131 I/M programs in the pinned corpus")
    source = args.manifest.parent / "riscv-tests"
    floating = [args.fp_dir / f"{family}-fadd.elf" for family in ("rv64uf", "rv64ud")]
    if args.prepare_only:
        args.fp_dir.mkdir(parents=True, exist_ok=True)
        for family, elf in zip(("rv64uf", "rv64ud"), floating):
            subprocess.run(["clang", "--target=riscv64-unknown-elf", "-march=rv64gc", "-mabi=lp64d",
                            "-mcmodel=medany", "-nostdlib", "-static", "-fuse-ld=lld",
                            "-Dglobal=weak", "-Dglobl=weak", "-I", str(source / "env/p"),
                            "-I", str(source / "isa/macros/scalar"), "-T", str(source / "env/p/link.ld"),
                            str(source / "isa" / family / "fadd.S"), "-o", str(elf)], check=True)
        return 0
    paths = [Path(entry["path"]) for entry in manifest] + floating
    if len(set(path.resolve() for path in paths)) != 133:
        parser.error("corpus must contain 133 distinct programs")
    for path in paths:
        if not path.is_file():
            parser.error(f"missing ELF: {path}; build the corpus and run --prepare-only first")
    command = ["systemd-run", "--user", "--scope", "-q", "-p", "MemoryMax=24G",
               "-p", "MemorySwapMax=0", "--", str(args.binary),
               *map(str, paths[args.part::args.parts])]
    return subprocess.run(command, check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
