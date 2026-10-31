"""CRC-checked flat-image interpreter workloads, timed without initialization."""
import datetime
import json
import os
import signal
from pathlib import Path
import statistics
import subprocess

import lib

COREMARK_REVISION = "1f483d5b8316753a742cbf5590caf5bd0a4e4777"


def measure(command, environment, timeout=7200):
    process = subprocess.Popen(command, env=environment, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, start_new_session=True)
    try:
        output, errors = process.communicate(timeout=timeout)
    except BaseException:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.communicate()
        raise
    if process.returncode:
        raise lib.BenchError(f"measurement failed: {output}\n{errors}")
    result = json.loads(output)
    if result["instructions"] <= 0 or result["seconds"] <= 0:
        raise lib.BenchError(f"invalid measurement: {result}")
    return result


def build_coremark(iterations=1):
    if iterations < 1:
        raise lib.BenchError("CoreMark iterations must be positive")
    root = Path(lib.REPO_DIR)
    source = root / "target/tcg/coremark"
    if not source.exists():
        subprocess.run(["git", "clone", "https://github.com/eembc/coremark.git", str(source)], check=True)
        subprocess.run(["git", "-C", str(source), "checkout", "--detach", COREMARK_REVISION], check=True)
    revision = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    dirty = subprocess.check_output(["git", "-C", str(source), "status", "--porcelain"], text=True)
    if revision != COREMARK_REVISION or dirty:
        raise lib.BenchError(f"CoreMark checkout must be clean at {COREMARK_REVISION}: {source}")
    output = root / "target/tcg/micro"
    output.mkdir(parents=True, exist_ok=True)
    port = root / "bench/rv64"
    flags = ["clang", "--target=riscv64-unknown-elf", "-march=rv64gc", "-mabi=lp64d", "-mcmodel=medany",
             "-O2", "-ffreestanding", "-fno-builtin", "-ffunction-sections", "-fdata-sections",
             "-I" + str(port), "-I" + str(source), f"-DITERATIONS={iterations}"]
    objects = []
    for name in ("core_main", "core_list_join", "core_matrix", "core_state", "core_util"):
        dest = output / (name + ".o")
        rename = ["-Dmain=unused_coremark_main"] if name == "core_main" else []
        subprocess.run([*flags, *rename, "-c", str(source / (name + ".c")), "-o", str(dest)], check=True)
        objects.append(str(dest))
    stem = "coremark" if iterations == 1 else f"coremark-{iterations}"
    elf = output / (stem + ".elf")
    subprocess.run([*flags, "-nostdlib", "-static", "-fuse-ld=lld", "-Wl,--gc-sections",
                    "-T", str(port / "link.ld"), str(port / "start.S"), str(port / "coremark.c"),
                    *objects, "-o", str(elf)], check=True)
    image = output / (stem + ".bin")
    subprocess.run(["llvm-objcopy", "-O", "binary", str(elf), str(image)], check=True)
    return image


def execution_environment(backend, inherited):
    environment = dict(inherited)
    for name in ("NO_MEMO", "NO_INDEXED", "SPECIALIZE", "WARM", "PROFILE", "PROGRESS", "TRACE_PC"):
        environment.pop("VMLAB_TCG_" + name, None)
    if backend == "interpreter-reference":
        environment["VMLAB_TCG_NO_MEMO"] = "1"
        environment["VMLAB_TCG_NO_INDEXED"] = "1"
    elif backend == "interpreter-specialized":
        environment["VMLAB_TCG_SPECIALIZE"] = "1"
        environment["VMLAB_TCG_WARM"] = "1"
    return environment


def run(args, commit, dirty, host, log):
    if args.arch not in ("all", "riscv64") or args.vcpus not in ("1", "1,4"):
        raise lib.BenchError("interpreter micro-benchmarks support riscv64 and one hart")
    if args.repeats < 1:
        raise lib.BenchError("repeats must be positive")
    pin = lib.pin_cpus(1, args.cpus)
    log(f"interpreter micro-benchmarks: {args.backend}, riscv64, pin={pin}")
    if args.dry_run:
        return 0
    subprocess.run(["cargo", "build", "--release", "-p", "vmlab-hv-tcg", "--example", "micro"], cwd=lib.REPO_DIR, check=True)
    specialized = args.backend == "interpreter-specialized"
    image = build_coremark(1000 if specialized else 1)
    binary = str(Path(lib.REPO_DIR) / "target/release/examples/micro")
    workloads = {"scalar_loop_3000": ["loop", "1000"],
                 "coremark_algorithms_1": ["image", str(image), "80001000", "10000000"]}
    if specialized:
        workloads = {"scalar_loop_30000000_warm": ["loop", "10000000"],
                     "coremark_algorithms_1000_warm": ["image", str(image), "80001000", "1000000000"]}
    environment = execution_environment(args.backend, os.environ)
    for name, command in workloads.items():
        samples = []
        raw = []
        noise = {"governor": lib.read_governor(), "loadavg_start": lib.read_loadavg(),
                 "pin": pin, "memory_max": lib.MEMORY_MAX, "host_cpus": os.cpu_count(),
                 "coremark_revision": COREMARK_REVISION, "backend_version": "guarded scalar blocks" if specialized else "generated IR interpreter",
                 "warmup_excluded": specialized}
        for repeat in range(args.repeats):
            result = measure([*lib.systemd_run_prefix(), *lib.taskset_prefix(pin), binary, *command], environment)
            if specialized and result["native_instructions"] == 0:
                raise lib.BenchError("specialized measurement executed no native instructions")
            raw.append(result)
            samples.append(result["seconds"])
            log(f"{name} repeat {repeat + 1}: {result['mips']:.6f} MIPS, {result['instructions']} instructions")
        if len({r["instructions"] for r in raw}) != 1:
            raise lib.BenchError(f"nondeterministic instruction count: {name}")
        noise["loadavg_end"] = lib.read_loadavg()
        record = {"recorded": datetime.datetime.now().isoformat(timespec="seconds"),
                  "commit": commit, "dirty": dirty, "host": host, "backend": args.backend,
                  "arch": "riscv64", "guest": "flat-rv64gc", "vcpus": 1,
                  "workload": name, "unit": "s", "repeats": args.repeats, "samples": samples,
                  "median": statistics.median(samples), "spread": max(samples) - min(samples),
                  "qemu_baseline": None, "env": noise, "measurements": raw}
        lib.append_history("riscv64", record)
    return 0
