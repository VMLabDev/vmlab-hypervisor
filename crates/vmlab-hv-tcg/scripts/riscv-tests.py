#!/usr/bin/env python3
"""Build upstream riscv-tests and run each flat image under the memory cap."""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import time


def run(args, timeout=120):
    process = subprocess.Popen(args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                               text=True, start_new_session=True)
    try:
        output, _ = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        output, _ = process.communicate()
        return 124, output + "\ntimeout"
    return process.returncode, output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("--runner", type=Path, default=Path("target/release/examples/flat"))
    parser.add_argument("--output", type=Path, default=Path("target/tcg/test-results"))
    parser.add_argument("--families", nargs="+", default=["rv64ui", "rv64um", "rv64ua", "rv64uf", "rv64ud", "rv64uc", "rv64si", "rv64mi"])
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    results = []
    for family in args.families:
        for source in sorted((args.source / "isa" / family).glob("*.S")):
            name = f"{family}-p-{source.stem}"
            elf = args.output / f"{name}.elf"
            image = args.output / f"{name}.bin"
            # LLVM rejects a weak declaration followed by a global declaration.
            # These standalone, single-object executables need no strong linkage;
            # keep all exported symbols weak without changing any instructions.
            command = ["clang", "--target=riscv64-unknown-elf", "-march=rv64gc", "-mabi=lp64d", "-Dglobal=weak", "-Dglobl=weak",
                       "-mcmodel=medany", "-nostdlib", "-static", "-fuse-ld=lld",
                       "-I", str(args.source / "env/p"), "-I", str(args.source / "isa/macros/scalar"),
                       "-T", str(args.source / "env/p/link.ld"), str(source), "-o", str(elf)]
            code, output = run(command)
            if code:
                status = "build-failed"
            else:
                subprocess.run(["llvm-objcopy", "-O", "binary", str(elf), str(image)], check=True)
                symbols = subprocess.check_output(["llvm-nm", "--format=posix", str(elf)], text=True)
                tohost = next(line.split()[2] for line in symbols.splitlines() if line.split()[0] == "tohost")
                start = time.monotonic()
                code, output = run(["systemd-run", "--user", "--scope", "-q", "-p", "MemoryMax=24G",
                                    "-p", "MemorySwapMax=0", "--", str(args.runner), str(image), tohost, "100000"])
                status = "pass" if code == 0 else "timeout" if code == 124 else "fail"
                output += f"\nwall_seconds={time.monotonic() - start:.6f}\n"
            (args.output / f"{name}.log").write_text(output)
            results.append({"name": name, "status": status, "exit_code": code, "output": output})
            (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
            print(f"{name}: {status}", flush=True)
    summary = {status: sum(r["status"] == status for r in results)
               for status in sorted({r["status"] for r in results})}
    print(json.dumps(summary), flush=True)
    raise SystemExit(0 if all(r["status"] == "pass" for r in results) and results else 1)


if __name__ == "__main__":
    main()
