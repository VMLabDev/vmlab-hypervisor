#!/usr/bin/env python3
"""Build the adapter from a configured sail-riscv CMake build, without editing it."""
import argparse
import concurrent.futures
import json
import pathlib
import subprocess
from compile_database import compile_command, select_entries

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("build", type=pathlib.Path)
parser.add_argument("output", type=pathlib.Path)
args = parser.parse_args()
build = args.build.resolve()
output = args.output.resolve()
output.mkdir(parents=True, exist_ok=True)
entries = json.loads((build / "compile_commands.json").read_text())
selected = select_entries(entries)

def compile_one(pair):
    index, entry = pair
    command = compile_command(entry)
    dest = output / f"{index}.o"
    command[command.index("-o") + 1] = str(dest)
    command = [arg for arg in command if not arg.startswith("-O")]
    command += ["-O0", "-fPIC"]
    subprocess.run(command, cwd=entry["directory"], check=True)
    return dest

with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
    objects = list(pool.map(compile_one, enumerate(selected)))
model_entry = next(e for e in selected if pathlib.Path(e["file"]).name == "sail_riscv_model.cpp")
flags = compile_command(model_entry)
includes = []
for index, flag in enumerate(flags):
    if flag.startswith("-I"):
        includes.append(flag)
    elif flag == "-isystem":
        includes.extend([flag, flags[index + 1]])
adapter = pathlib.Path(__file__).resolve().parents[1] / "native/oracle.cpp"
subprocess.run(["c++", "-std=c++17", "-fPIC", "-O0", *includes, "-c", str(adapter),
                "-o", str(output / "adapter.o")], check=True)
wraps = ["emulator_read_mem", "emulator_read_mem_ifetch", "emulator_read_mem_exclusive",
         "emulator_write_mem", "emulator_write_mem_exclusive"]
subprocess.run(["c++", "-shared", "-Wl,-Bsymbolic", "-Wl,--no-undefined",
                *(f"-Wl,--wrap={name}" for name in wraps),
                str(output / "adapter.o"), *map(str, objects), "-lgmp", "-lz",
                "-o", str(output / "libvmlab_sail_oracle.so")], check=True)
print(output / "libvmlab_sail_oracle.so", flush=True)
