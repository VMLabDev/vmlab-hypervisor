#!/usr/bin/env python3
"""Print the per-workload trend from bench/history (D2).

For every (backend, guest, vcpus, workload) configuration of every arch, the
series of recorded medians with spread and the move against the first and the
previous entry. When both backends have history for a configuration, the
vmlab-hv/QEMU ratio prints against D1's bar (at least 100 % of QEMU TCG).
"""

import argparse
import sys

import lib

ARCHES = ("aarch64", "riscv64")


def parse_args(argv):
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--arch", default="all", help="all, or aarch64,riscv64")
    p.add_argument("--backend", default="all", help="all, qemu, or vmlab-hv")
    p.add_argument("--guest", default=None, help="filter by template name")
    p.add_argument("--limit", type=int, default=0, help="show only the last N entries")
    args = p.parse_args(argv)
    if args.guest == "all":
        args.guest = None
    return args


def groups(records, args):
    out = {}
    for rec in records:
        if args.guest and rec.get("guest") != args.guest:
            continue
        if args.backend != "all" and rec.get("backend") != args.backend:
            continue
        key = (rec["backend"], rec["guest"], rec["vcpus"], rec["workload"])
        out.setdefault(key, []).append(rec)
    return out


def pct(a, b):
    if not b:
        return "n/a"
    return f"{(a - b) / b * 100:+.1f}%"


def print_arch(arch, args):
    records = lib.read_history(arch)
    if not records:
        print(f"{arch}: no history yet (run `just bench-run qemu {arch}`)")
        return
    grouped = groups(records, args)
    print(f"=== {arch} ===")
    for key in sorted(grouped):
        backend, guest, vcpus, workload = key
        entries = grouped[key]
        if args.limit:
            entries = entries[-args.limit :]
        first = entries[0]["median"]
        print(f"{backend:>9}  {guest}  vcpus={vcpus}  {workload}  (unit: s)")
        prev = None
        for rec in entries:
            moves = f"vs first {pct(rec['median'], first)}"
            if prev is not None:
                moves += f", vs prev {pct(rec['median'], prev)}"
            print(
                f"  {rec['recorded']}  {rec['commit'][:9]}"
                f"{'*' if rec.get('dirty') else ' '}"
                f"  median {rec['median']:8.2f}  spread {rec['spread']:6.2f}"
                f"  {moves}"
            )
            prev = rec["median"]
        if backend == "vmlab-hv":
            base = lib.latest_baseline(records, arch, guest, vcpus, workload)
            if base:
                ratio = entries[-1]["median"] / base["median"]
                bar = "meets" if ratio <= 1.0 else "BELOW"
                print(
                    f"  D1 bar: last hv median {entries[-1]['median']:.2f}s vs"
                    f" qemu {base['median']:.2f}s ({base['recorded']})"
                    f" = {ratio:.2f}x -> {bar} D1 (>=1.00x)"
                )
        print()


def main(argv=None):
    args = parse_args(argv)
    arches = list(ARCHES) if args.arch == "all" else [a.strip() for a in args.arch.split(",")]
    for arch in arches:
        print_arch(arch, args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
