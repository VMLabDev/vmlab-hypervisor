#!/usr/bin/env python3
"""Fetch pinned compliance sources and build physical-mode RV64I/M ELF tests."""
import argparse
import json
import pathlib
import shutil
import subprocess

SOURCES = {
    "riscv-tests": ("https://github.com/riscv-software-src/riscv-tests.git", "3df703cd52d5f0d8224e3c50bf03c6cb70efff98"),
    "riscv-arch-test": ("https://github.com/riscv-non-isa/riscv-arch-test.git", "eb66181dd27ff7847e2c3a010705b13490b0bf75"),
}
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("target", type=pathlib.Path, help="checkout and build root under target/")
parser.add_argument("--fetch-only", action="store_true")
parser.add_argument("--gnu-as", type=pathlib.Path, help="GNU RISC-V assembler for Clang's external assembler mode")
args = parser.parse_args()
root = args.target.resolve()
if "target" not in root.parts:
    parser.error("corpus artifacts must be under target/")
root.mkdir(parents=True, exist_ok=True)
for name, (url, revision) in SOURCES.items():
    checkout = root / name
    if not checkout.exists():
        subprocess.run(["git", "clone", url, str(checkout)], check=True)
    dirty = subprocess.check_output(["git", "-C", str(checkout), "status", "--porcelain"], text=True)
    if dirty:
        raise SystemExit(f"refusing to change dirty checkout: {checkout}")
    subprocess.run(["git", "-C", str(checkout), "checkout", "--detach", revision], check=True)
    if name == "riscv-tests":
        subprocess.run(["git", "-C", str(checkout), "submodule", "update", "--init", "--recursive"], check=True)
if args.fetch_only:
    raise SystemExit(0)
gcc = next((shutil.which(c) for c in ["riscv64-unknown-elf-gcc", "riscv64-elf-gcc", "riscv64-linux-gnu-gcc"] if shutil.which(c)), None)
if gcc:
    compiler = [gcc]
elif shutil.which("clang") and shutil.which("ld.lld"):
    compiler = [shutil.which("clang"), "--target=riscv64-unknown-elf", "-fuse-ld=lld", "-Wno-unused-command-line-argument"]
    if args.gnu_as:
        tools = root / "tools"
        tools.mkdir(exist_ok=True)
        assembler = tools / "as"
        if assembler.is_symlink() and assembler.resolve() != args.gnu_as.resolve():
            raise SystemExit(f"assembler symlink points elsewhere: {assembler}")
        if not assembler.exists():
            assembler.symlink_to(args.gnu_as.resolve())
        compiler += ["-fno-integrated-as", "-B" + str(tools)]
else:
    raise SystemExit("no RV64 cross compiler: install riscv64-unknown-elf-gcc or clang with ld.lld; pinned sources fetched")
package = pathlib.Path(__file__).resolve().parents[1]
output = root / "bin"
output.mkdir(exist_ok=True)
manifest = []
for suite, groups in [("riscv-tests", ["rv64ui", "rv64um"]), ("riscv-arch-test", ["I", "M"])]:
    checkout = root / suite
    for group in groups:
        if suite == "riscv-tests":
            sources = checkout / "isa" / group
            flags = ["-I" + str(checkout / "env/p"), "-I" + str(checkout / "isa/macros/scalar"),
                     "-T" + str(checkout / "env/p/link.ld")]
        else:
            sources = checkout / "riscv-test-suite/rv64i_m" / group / "src"
            flags = ["-I" + str(checkout / "riscv-test-suite/env"), "-I" + str(package / "corpus"),
                     "-T" + str(package / "corpus/link.ld"), "-DTEST_CASE_1", "-DXLEN=64", "-DFLEN=64"]
        for source in sorted(sources.glob("*.S")):
            dest = output / f"{suite}-{group}-{source.stem}.elf"
            command = [*compiler, "-march=rv64g", "-mabi=lp64d", "-mcmodel=medany", "-nostdlib", "-static",
                       "-Wl,--no-relax", *flags, str(source), "-o", str(dest)]
            print(dest.name, flush=True)
            subprocess.run(command, check=True)
            manifest.append({"path": str(dest), "suite": suite, "source": str(source), "revision": SOURCES[suite][1]})
(root / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
print(f"built {len(manifest)} ELFs; manifest: {root / 'manifest.json'}")
