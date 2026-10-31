#!/usr/bin/env python3
"""Run the D2 benchmark suite: backend × arch × vcpus × N repeats → history.

One VM at a time, each under the systemd-run memory cap and a taskset pin,
release → agent ready → in-guest workloads, all timed host-side. After the
repeats of one configuration, one JSON line per workload is appended to
bench/history/<arch>.jsonl (format in bench/README.md).

Exit code: 0 on success and on a skipped backend (with the reason printed);
1 on any measurement failure.
"""

import argparse
import datetime
import json
import os
import shutil
import subprocess
import sys
import time

import lib
from backends import HvBackend, QemuBackend

# Generous ceilings; a TCG riscv64 boot is ~1 min under QEMU and the guest
# workloads run for minutes, so these only catch hangs. The future
# interpreter-class vmlab-hv may need `--timeout-scale`.
BOOT_DEADLINE = 1800.0
PREP_TIMEOUT = 3600
WORKLOAD_TIMEOUT = {"sha256_256M": 3600, "xz_9_32M": 7200}
VERIFY_TIMEOUT = 3600
IDLE_TIMEOUT = 600
SHUTDOWN_TIMEOUT = 300


def parse_args(argv):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--backend", choices=("qemu", "vmlab-hv", "interpreter", "interpreter-reference"), default="qemu")
    p.add_argument("--arch", default="all", help="all, or aarch64,riscv64")
    p.add_argument("--vcpus", default="1,4", help="comma-separated vCPU counts")
    p.add_argument("--repeats", type=int, default=3, help="repeats per configuration")
    p.add_argument("--guest", default="debian-13", help="template name")
    p.add_argument("--cpus", default=None, help="taskset window override (e.g. 16-19)")
    p.add_argument("--hv-bin", default=os.path.join(lib.REPO_DIR, "target/release/vmlab-hv"))
    p.add_argument("--timeout-scale", type=float, default=1.0)
    p.add_argument("--keep", action="store_true", help="keep per-run scratch dirs")
    p.add_argument("--dry-run", action="store_true", help="print the plan, boot nothing")
    return p.parse_args(argv)


def resolve_arches(spec):
    if spec == "all":
        return ["aarch64", "riscv64"]
    arches = [a.strip() for a in spec.split(",") if a.strip()]
    for a in arches:
        if a not in lib.FIRMWARE:
            raise lib.BenchError(f"unknown arch {a!r} (want aarch64 or riscv64)")
    return arches


def git_commit():
    sha = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=lib.REPO_DIR, capture_output=True, text=True
    ).stdout.strip()
    dirty = subprocess.run(
        ["git", "status", "--porcelain", "--untracked-files=no"],
        cwd=lib.REPO_DIR,
        capture_output=True,
        text=True,
    ).stdout.strip()
    return sha, bool(dirty)
    return sha, bool(dirty)


def log(msg):
    print(f"[{datetime.datetime.now().strftime('%H:%M:%S')}] {msg}", flush=True)


def make_backend(args, run_dir, arch, smp, pin, overlay, code_fd, vars_fd):
    if args.backend == "qemu":
        return QemuBackend(run_dir, arch, smp, pin, overlay, code_fd, vars_fd, log)
    return HvBackend(
        run_dir, arch, smp, pin, overlay, code_fd, vars_fd, log, args.hv_bin
    )


def one_repeat(args, arch, smp, pin, version, index):
    """One VM: launch → ready → workloads → clean shutdown. Returns samples."""
    stamp = datetime.datetime.now().strftime("%Y%m%dT%H%M%S")
    run_dir = os.path.join(
        lib.RUN_DIR, f"{stamp}-{args.backend}-{arch}-smp{smp}-r{index}"
    )
    shutil.rmtree(run_dir, ignore_errors=True)
    os.makedirs(run_dir)
    overlay, code_fd, vars_fd = lib.prepare_run_dir(run_dir, arch, args.guest, version)
    backend = make_backend(args, run_dir, arch, smp, pin, overlay, code_fd, vars_fd)

    result = {"run": run_dir, "arch": arch, "vcpus": smp, "backend": args.backend}
    session = backend.launch()
    agent = None
    try:
        backend.release_cpus()
        result["t_release"] = round(session.elapsed(), 2)
        log(f"released CPUs after {result['t_release']}s; waiting for agent")

        deadline = BOOT_DEADLINE * args.timeout_scale
        agent, hello = lib.wait_for_agent(
            backend.agent_socket(), deadline, vm_exited=session.exit_code
        )
        result["boot_to_agent_ready"] = round(session.elapsed(), 2)
        result["agent_hello"] = {
            k: hello.get(k) for k in ("agent_version", "os", "features")
        }
        log(f"agent ready after {result['boot_to_agent_ready']}s")

        def timed(name, argv, timeout):
            t = time.monotonic()
            chan = agent.exec(argv, timeout, IDLE_TIMEOUT * args.timeout_scale)
            dt = time.monotonic() - t
            result[name] = {
                "secs": round(dt, 2),
                "code": chan["code"],
                "out": chan["data"].decode(errors="replace")[-400:],
                "err": chan["stderr"].decode(errors="replace")[-200:],
            }
            if chan["code"] != 0:
                raise lib.BenchError(
                    f"{name} exited {chan['code']}: "
                    f"{chan['stderr'].decode(errors='replace')[-400:]}"
                )
            log(f"  {name}: {dt:.2f}s")
            return dt

        # Fixed inputs, untimed (notes/03), then the two timed workloads.
        timed("prep", lib.PREP_ARGV, PREP_TIMEOUT * args.timeout_scale)
        sha_secs = timed(
            "sha256_256M", lib.WORKLOAD_ARGV["sha256_256M"],
            WORKLOAD_TIMEOUT["sha256_256M"] * args.timeout_scale,
        )
        xz_secs = timed(
            "xz_9_32M", lib.WORKLOAD_ARGV["xz_9_32M"],
            WORKLOAD_TIMEOUT["xz_9_32M"] * args.timeout_scale,
        )

        # Determinism gate: the inputs are fixed, so the digest must be too,
        # and the guest must really see `smp` CPUs.
        timed("verify", lib.VERIFY_ARGV, VERIFY_TIMEOUT * args.timeout_scale)
        out = result["verify"]["out"]
        if "XZ-OK" not in out:
            raise lib.BenchError(f"verify failed: {out!r}")
        nproc = int(out.strip().splitlines()[-2])
        if nproc != smp:
            raise lib.BenchError(f"guest sees {nproc} CPUs, expected {smp}")
        digest = result["sha256_256M"]["out"].split()[0]
        result["sha256_digest"] = digest

        result["samples"] = {
            "boot_to_agent_ready": result["boot_to_agent_ready"],
            "sha256_256M": round(sha_secs, 2),
            "xz_9_32M": round(xz_secs, 2),
        }
        return result, backend, session, agent, digest
    finally:
        # Always leave nothing running. The graceful paths are best effort:
        # a failure in them must not skip the kill.
        try:
            if agent is not None:
                agent.shutdown()
                agent.close()
            if args.backend == "vmlab-hv":
                backend.quit()
            code = session.wait_exit(SHUTDOWN_TIMEOUT)
            if code is None:
                log(f"VM did not exit after powerdown within {SHUTDOWN_TIMEOUT}s")
        except Exception as e:  # noqa: BLE001 - cleanup must reach the kill
            log(f"note: graceful shutdown path failed ({e}); killing")
        session.kill()
        result["vm_exit"] = session.exit_code()
        if result["vm_exit"] != 0:
            log(f"warning: VM exit code {result['vm_exit']}")
        # Bulky scratch: the overlay grew by everything the guest wrote; the
        # serial log and result.json stay.
        for name in ("overlay.qcow2", "vars.fd"):
            path = os.path.join(run_dir, name)
            if os.path.exists(path) and not args.keep:
                os.remove(path)
        with open(os.path.join(run_dir, "result.json"), "w") as f:
            json.dump(result, f, indent=1)


def run_config(args, arch, smp, pin, version, commit, dirty, host):
    """N repeats of one configuration → one history line per workload."""
    all_samples = {w: [] for w in lib.WORKLOADS}
    digests = set()
    env = {
        "governor": lib.read_governor(),
        "loadavg_start": lib.read_loadavg(),
        "pin": pin,
        "memory_max": lib.MEMORY_MAX,
        "template": version,
        "host_cpus": os.cpu_count(),
    }
    for i in range(1, args.repeats + 1):
        log(f"=== {args.backend} {arch} smp={smp} {args.guest} repeat {i}/{args.repeats}")
        result, backend, session, agent, digest = one_repeat(
            args, arch, smp, pin, version, i
        )
        env["loadavg_end"] = lib.read_loadavg()
        env["backend_version"] = backend.backend_version()
        for w in lib.WORKLOADS:
            all_samples[w].append(result["samples"][w])
        digests.add(digest)
        log(f"repeat {i}: {result['samples']}")

    if len(digests) != 1:
        raise lib.BenchError(f"sha256 digest changed across repeats: {digests}")

    recorded = datetime.datetime.now().isoformat(timespec="seconds")
    existing = lib.read_history(arch)
    appended = []
    for w in lib.WORKLOADS:
        median, spread = lib.median_and_spread(all_samples[w])
        record = {
            "recorded": recorded,
            "commit": commit,
            "dirty": dirty,
            "host": host,
            "backend": args.backend,
            "arch": arch,
            "guest": args.guest,
            "vcpus": smp,
            "workload": w,
            "unit": lib.UNIT,
            "repeats": args.repeats,
            "samples": all_samples[w],
            "median": median,
            "spread": spread,
            "qemu_baseline": None,
            "env": env,
        }
        if args.backend == "vmlab-hv":
            record["qemu_baseline"] = lib.latest_baseline(
                existing, arch, args.guest, smp, w
            )
            if record["qemu_baseline"] is None:
                log(f"warning: no QEMU baseline in history for {arch} smp={smp} {w}")
        lib.append_history(arch, record)
        appended.append(record)
    return appended


def main(argv=None):
    args = parse_args(argv)
    if args.backend in ("interpreter", "interpreter-reference"):
        import micro
        commit, dirty = git_commit()
        return micro.run(args, commit, dirty, os.uname().nodename, log)
    if args.repeats < 2:
        log("note: --repeats < 2 leaves no spread; bench-check treats that as zero noise")
    arches = resolve_arches(args.arch)
    vcpus = [int(v) for v in args.vcpus.split(",") if v.strip()]
    commit, dirty = git_commit()
    host = os.uname().nodename

    for arch in arches:
        version = lib.latest_template(arch, args.guest)
        for smp in vcpus:
            pin = lib.pin_cpus(smp, args.cpus)
            log(f"### {args.backend} {arch} smp={smp} {args.guest}@{version} pin={pin} "
                f"governor={lib.read_governor()} load={lib.read_loadavg()}")
            if args.dry_run:
                continue
            try:
                records = run_config(args, arch, smp, pin, version, commit, dirty, host)
                for r in records:
                    log(f"history: {r['workload']} median={r['median']}s "
                        f"spread={r['spread']}s")
            except lib.BackendUnsupported as skip:
                log(f"SKIP {skip.backend} {arch} smp={smp}: {skip.reason}")
                log("(no history recorded for this configuration)")
    if args.dry_run:
        return 0
    log("done")
    return 0


if __name__ == "__main__":
    sys.exit(main())
