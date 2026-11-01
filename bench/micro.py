"""CRC-checked flat-image interpreter workloads, timed without initialization.

The `qemu` backend (guest `flat-rv64gc`) runs the same flat rv64gc images
under qemu-system-riscv64 TCG on the minimal spike machine, whose HTIF
device honours the images' tohost exit store, so the interpreter and QEMU
numbers share workload keys, binaries and stop condition."""
import datetime
import json
import os
import signal
import tempfile
import time
from pathlib import Path
import statistics
import subprocess

import lib

COREMARK_REVISION = "1f483d5b8316753a742cbf5590caf5bd0a4e4777"
OS_WORKLOADS = ("ram", "ram-after-mmio", "same-page-data", "self-modifying",
                "sv39-tlb", "sv48-tlb", "m-traps", "s-traps", "clint-timer",
                "clint-software", "mmio", "large-code", "fresh-code", "cache-pressure",
                "store-heavy", "store-heavy-after-mmio")
OS_ROUNDS = {case: (1024 if case.startswith("ram") else
                    1 if case in ("self-modifying", "large-code", "fresh-code", "cache-pressure") else 8)
             for case in OS_WORKLOADS}

# The flat QEMU machine: spike (HTIF tohost/fromhost exit), one rv64 hart,
# TCG, no devices the images do not touch. The kernel is the ELF so QEMU
# can find the tohost symbol; its load image is byte-checked against the
# flat .bin the interpreter runs.
QEMU_BINARY = "qemu-system-riscv64"
QEMU_MACHINE_ARGS = ("-machine", "spike", "-cpu", "rv64", "-accel", "tcg",
                     "-smp", "1", "-m", "128M", "-bios", "none")
QEMU_MEMORY_MAX = "8G"  # the wave-4 brief's cap for one QEMU TCG run

# The scalar-loop workload: `micro loop N` (examples/micro.rs) executes
# exactly LOOP_WORDS N times with x2 preset to N. bench/rv64/loop.S embeds
# the same block behind a minimal li x2 stub and a fixed tohost exit;
# loop_expected_instructions() is the count the interpreter must confirm.
LOOP_WORDS = (0x00108093, 0xFFF10113, 0xFE011CE3)
LOOP_ITERATIONS = 10_000_000
STARTUP_ITERATIONS = 1
# A QEMU sample's wall is ~35 ms with a few ms of scheduler and HTIF-poll
# jitter, and the 30M-instruction loop executes in ~4 ms: the net needs
# enough samples for a stable median, so flat-QEMU runs use at least this
# many repeats (each sample costs ~40 ms).
FLAT_QEMU_MIN_REPEATS = 15


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
    port = root / "bench/rv64"
    output = root / "target/tcg/micro"
    output.mkdir(parents=True, exist_ok=True)
    flags = [*rv64_flags(), "-I" + str(port), "-I" + str(source), f"-DITERATIONS={iterations}"]
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

def rv64_flags():
    """The flags every flat rv64gc image in this suite builds with."""
    return ["clang", "--target=riscv64-unknown-elf", "-march=rv64gc", "-mabi=lp64d",
            "-mcmodel=medany", "-O2", "-ffreestanding", "-fno-builtin",
            "-ffunction-sections", "-fdata-sections"]


def build_loop(iterations):
    """bench/rv64/loop.S as an ELF (for QEMU's HTIF symbols) plus flat image."""
    if iterations < 1:
        raise lib.BenchError("loop iterations must be positive (0 never exits the bne)")
    root = Path(lib.REPO_DIR)
    output = root / "target/tcg/micro"
    output.mkdir(parents=True, exist_ok=True)
    port = root / "bench/rv64"
    elf = output / f"loop-{iterations}.elf"
    subprocess.run([*rv64_flags(), f"-DLOOP_ITERATIONS={iterations}", "-nostdlib",
                    "-static", "-fuse-ld=lld", "-Wl,--gc-sections",
                    "-T", str(port / "link.ld"), str(port / "loop.S"), "-o", str(elf)],
                   check=True)
    image = output / f"loop-{iterations}.bin"
    subprocess.run(["llvm-objcopy", "-O", "binary", str(elf), str(image)], check=True)
    check_loop_image(image.read_bytes())
    return elf, image


def check_loop_image(data):
    """The interpreter's loop block, exactly once, behind the entry stub.

    The stub's `lui %hi/addi %lo` x2 setup collapses to one `li` when the
    iteration count fits a signed 12-bit immediate, so the block's offset
    is 4 or 8; the whole image is scanned so a stray second copy anywhere
    fails the build too."""
    expected = b"".join(word.to_bytes(4, "little") for word in LOOP_WORDS)
    hits = [offset for offset in range(0, len(data) - len(expected) + 1, 4)
            if data[offset:offset + len(expected)] == expected]
    if hits not in ([4], [8]):
        raise lib.BenchError(f"loop image lacks the single interpreter loop block: {hits}")


def loop_expected_instructions(iterations):
    """3*N loop instructions plus the stub and the tohost exit around it.

    The stub is the minimal `li x2, N` (one instruction up to 2047, two
    above), the exit is la (norelax auipc+addi), li, slli, ori and the sd
    the interpreter stops on."""
    stub = 1 if iterations <= 0x7ff else 2
    return 3 * iterations + stub + 6


def elf_loads_image(elf, image):
    """QEMU boots the ELF; assert its load image is the interpreter's .bin."""
    with tempfile.NamedTemporaryFile(suffix=".bin") as flat:
        subprocess.run(["llvm-objcopy", "-O", "binary", str(elf), flat.name], check=True)
        if Path(flat.name).read_bytes() != Path(image).read_bytes():
            raise lib.BenchError(f"{elf} load image differs from {image}")


def qemu_version():
    return subprocess.check_output([QEMU_BINARY, "--version"], text=True).splitlines()[0].strip()


def qemu_argv(kernel):
    return [QEMU_BINARY, *QEMU_MACHINE_ARGS, "-kernel", str(kernel),
            "-display", "none", "-serial", "none", "-monitor", "none"]


def measure_qemu(command, environment, timeout=3600):
    """Wall time of one pinned QEMU run, spawn to HTIF exit."""
    start = time.monotonic()
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
    wall = time.monotonic() - start
    if process.returncode:
        raise lib.BenchError(f"qemu run failed ({process.returncode}): {output}\n{errors}")
    return wall, output, errors


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


def run_qemu(args, commit, dirty, host, log):
    """The interpreter micro-benchmarks under QEMU TCG, same workload keys.

    QEMU boots each ELF on the minimal spike machine and the guest's own
    tohost store ends the run, so the timed region is guest execution plus
    QEMU teardown. A one-iteration loop image measures spawn-plus-teardown
    separately; its median is subtracted from every workload sample and
    both numbers are recorded. MIPS uses the instruction count of the same
    flat image measured under the interpreter once per run."""
    if args.arch not in ("all", "riscv64") or (args.vcpus or "1,4") not in ("1", "1,4"):
        raise lib.BenchError("qemu micro-benchmarks support riscv64 and one hart")
    if args.repeats < 1:
        raise lib.BenchError("repeats must be positive")
    pin = lib.pin_cpus(1, args.cpus)
    log(f"qemu TCG micro-benchmarks: flat-rv64gc, pin={pin}")
    if args.dry_run:
        return 0
    subprocess.run(["cargo", "build", "--release", "-p", "vmlab-hv-tcg", "--example", "micro"],
                   cwd=lib.REPO_DIR, check=True)
    binary = str(Path(lib.REPO_DIR) / "target/release/examples/micro")
    count_environment = execution_environment("interpreter-specialized", os.environ)
    qemu_environment = execution_environment("qemu", os.environ)
    loop_elf, loop_image = build_loop(LOOP_ITERATIONS)
    startup_elf, _ = build_loop(STARTUP_ITERATIONS)
    coremark_image = build_coremark(1000)
    coremark_elf = coremark_image.with_suffix(".elf")
    elf_loads_image(coremark_elf, coremark_image)
    elf_loads_image(loop_elf, loop_image)
    workloads = {"scalar_loop_30000000_warm":
                     (loop_elf, loop_image, loop_expected_instructions(LOOP_ITERATIONS),
                      str(loop_expected_instructions(LOOP_ITERATIONS) + 1024)),
                 "coremark_algorithms_1000_warm":
                     (coremark_elf, coremark_image, None, "400000000")}
    startup_samples = []
    # One unrecorded run of each image first: the recorded walls then miss
    # the host's cold page-cache cost of QEMU's own binary and the ELF,
    # which is not guest execution. QEMU's translation cache is per
    # process, so nothing about the guest itself is warmed across runs.
    measure_qemu([*lib.systemd_run_prefix(QEMU_MEMORY_MAX),
                  *lib.taskset_prefix(pin), *qemu_argv(startup_elf)], qemu_environment)
    repeats = max(args.repeats, FLAT_QEMU_MIN_REPEATS)
    for repeat in range(repeats):
        wall, _, _ = measure_qemu([*lib.systemd_run_prefix(QEMU_MEMORY_MAX),
                                   *lib.taskset_prefix(pin), *qemu_argv(startup_elf)],
                                  qemu_environment)
        startup_samples.append(wall)
    startup = statistics.median(startup_samples)
    log(f"qemu startup baseline: median {startup:.6f} s over {repeats} repeats")
    for name, (elf, image, expected, limit) in workloads.items():
        measure_qemu([*lib.systemd_run_prefix(QEMU_MEMORY_MAX),
                      *lib.taskset_prefix(pin), *qemu_argv(elf)], qemu_environment)
        counted = measure([*lib.systemd_run_prefix(), *lib.taskset_prefix(pin), binary,
                           "image", str(image), "80001000", limit], count_environment)
        instructions = counted["instructions"]
        if expected is not None and instructions != expected:
            raise lib.BenchError(f"{name}: interpreter count {instructions} != {expected}")
        samples = []
        raw = []
        noise = {"governor": lib.read_governor(), "loadavg_start": lib.read_loadavg(),
                 "pin": pin, "memory_max": QEMU_MEMORY_MAX, "host_cpus": os.cpu_count(),
                 "coremark_revision": COREMARK_REVISION, "backend_version": qemu_version(),
                 "warmup_excluded": False,
                 "qemu": {"machine": "spike", "cpu": "rv64", "accel": "tcg", "memory": "128M",
                          "kernel": "elf with HTIF tohost exit"},
                 "startup_samples": startup_samples, "startup_median": startup,
                 "instruction_count": instructions,
                 "instruction_count_source": "interpreter-specialized run of the same flat image",
                 "instruction_count_seconds": counted["seconds"]}
        for repeat in range(repeats):
            wall, _, _ = measure_qemu([*lib.systemd_run_prefix(QEMU_MEMORY_MAX),
                                       *lib.taskset_prefix(pin), *qemu_argv(elf)],
                                      qemu_environment)
            net = wall - startup
            samples.append(net)
            # A single sample below the startup median is expected noise
            # when the workload executes for only a few milliseconds; the
            # median must still be positive or the run is not a measurement.
            raw.append({"wall_seconds": wall, "net_seconds": net,
                        "instructions": instructions,
                        "mips": instructions / net / 1e6 if net > 0 else None})
            mips = "" if net <= 0 else f" {raw[-1]['mips']:.2f} MIPS,"
            log(f"{name} repeat {repeat + 1}:{mips} {net:.6f} s net ({wall:.6f} s wall)")
        if statistics.median(samples) <= 0:
            raise lib.BenchError(f"{name}: median net {statistics.median(samples):.6f} s"
                                 f" does not exceed the startup median {startup:.6f} s")
        noise["loadavg_end"] = lib.read_loadavg()
        record = {"recorded": datetime.datetime.now().isoformat(timespec="seconds"),
                  "commit": commit, "dirty": dirty, "host": host, "backend": "qemu",
                  "arch": "riscv64", "guest": "flat-rv64gc", "vcpus": 1,
                  "workload": name, "unit": "s", "repeats": repeats, "samples": samples,
                  "median": statistics.median(samples), "spread": max(samples) - min(samples),
                  "qemu_baseline": None, "env": noise, "measurements": raw}
        lib.append_history("riscv64", record)
    return 0


def run(args, commit, dirty, host, log):
    os_suite = args.suite == "os"
    if os_suite and args.backend != "interpreter-specialized":
        raise lib.BenchError("--suite os requires --backend interpreter-specialized")
    if args.backend == "qemu":
        return run_qemu(args, commit, dirty, host, log)
    if args.arch not in ("all", "riscv64") or (args.vcpus or "1,4") not in ("1", "1,4"):
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
    workloads = {name: ([binary, *command], "flat-rv64gc")
                 for name, command in workloads.items()}
    if os_suite:
        subprocess.run(["cargo", "build", "--release", "-p", "vmlab-hv-vmm",
                        "--features", "tcg-bench", "--example", "os-workload"],
                       cwd=lib.REPO_DIR, check=True)
        os_binary = str(Path(lib.REPO_DIR) / "target/release/examples/os-workload")
        workloads.update({f"os_v1_{case}_{OS_ROUNDS[case]}":
                          ([os_binary, case, str(OS_ROUNDS[case])], "os-rv64gc")
                          for case in OS_WORKLOADS})
        if args.replay_fixture:
            fixture = Path(args.replay_fixture)
            manifest = json.loads((fixture / "manifest.json").read_text())
            digest = manifest["sha256"]
            if len(digest) != 64 or any(character not in "0123456789abcdef" for character in digest):
                raise lib.BenchError("invalid replay fixture digest")
            workloads[f"edk2_slice_{digest}_8192_128_warm"] = (
                [os_binary, "replay", str(fixture), "8192", "128"], "os-rv64gc")
    environment = execution_environment(args.backend, os.environ)
    coremark_mips = None
    for name, (command, guest) in workloads.items():
        samples = []
        raw = []
        noise = {"governor": lib.read_governor(), "loadavg_start": lib.read_loadavg(),
                 "pin": pin, "memory_max": lib.MEMORY_MAX, "host_cpus": os.cpu_count(),
                 "coremark_revision": COREMARK_REVISION, "backend_version": "guarded scalar blocks" if specialized else "generated IR interpreter",
                 "warmup_excluded": specialized and not name.startswith("os_v1_fresh-code_")}
        for repeat in range(args.repeats):
            result = measure([*lib.systemd_run_prefix(), *lib.taskset_prefix(pin), *command], environment)
            if specialized and guest == "flat-rv64gc" and result["native_instructions"] == 0:
                raise lib.BenchError("specialized measurement executed no native instructions")
            raw.append(result)
            samples.append(result["seconds"])
            log(f"{name} repeat {repeat + 1}: {result['mips']:.6f} MIPS, {result['instructions']} instructions")
        if len({r["instructions"] for r in raw}) != 1:
            raise lib.BenchError(f"nondeterministic instruction count: {name}")
        median_mips = statistics.median(result["mips"] for result in raw)
        if name == "coremark_algorithms_1000_warm":
            coremark_mips = median_mips
        if guest == "os-rv64gc" and coremark_mips is not None:
            ratio = median_mips / coremark_mips
            noise["matched_coremark_mips"] = coremark_mips
            noise["coremark_ratio"] = ratio
            log(f"{name}: {ratio:.3f}x matched CoreMark; "
                f"{'meets' if ratio >= 0.5 else 'FAILS'} the 0.5x target")
        noise["loadavg_end"] = lib.read_loadavg()
        record = {"recorded": datetime.datetime.now().isoformat(timespec="seconds"),
                  "commit": commit, "dirty": dirty, "host": host, "backend": args.backend,
                  "arch": "riscv64", "guest": guest, "vcpus": 1,
                  "workload": name, "unit": "s", "repeats": args.repeats, "samples": samples,
                  "median": statistics.median(samples), "spread": max(samples) - min(samples),
                  "qemu_baseline": None, "env": noise, "measurements": raw}
        lib.append_history("riscv64", record)
    return 0
