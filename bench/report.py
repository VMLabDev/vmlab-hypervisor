#!/usr/bin/env python3
"""Print the per-workload trend from bench/history (D2, D28).

For every (backend, guest, vcpus, workload) configuration of every arch, the
series of recorded medians with spread and the move against the first and the
previous entry. When both backends have history for a configuration, the
vmlab-hv/QEMU ratio prints against the phase's bar: TCG arches hold D1's
"at least 100 % of QEMU TCG" (a ≥1.00x speed ratio on seconds), x86_64 KVM
holds HV-D28 (boot no slower than QEMU, throughput at least 80 % of QEMU's).
"""

import argparse
import sys

import lib

ARCHES = ("aarch64", "riscv64", "x86_64")

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
        unit = entries[-1].get("unit", "s")
        print(f"{backend:>9}  {guest}  vcpus={vcpus}  {workload}  (unit: {unit})")
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
        if backend == "vmlab-hv" or backend.startswith("interpreter"):
            base = lib.latest_baseline(records, arch, guest, vcpus, workload)
            if base:
                ratio = entries[-1]["median"] / base["median"]
                if arch == "x86_64":
                    print(bar_line_d28(workload, ratio, entries[-1]["median"], base))
                else:
                    bar = "meets" if ratio <= 1.0 else "BELOW"
                    print(
                        f"  D1 bar: last {backend} median {entries[-1]['median']:.2f}s vs"
                        f" qemu {base['median']:.2f}s ({base['recorded']})"
                        f" = {ratio:.2f}x -> {bar} D1 (>=1.00x)"
                    )
        print()


def bar_line_d28(workload, ratio, hv_median, base):
    """The HV-D28 verdict line for one workload's hv/qemu ratio."""
    bar = lib.D28_BAR.get(workload)
    hv = f"{hv_median:.2f}"
    qemu = f"{base['median']:.2f}"
    provenance = f"({base['recorded']})"
    if bar is None:
        return (f"  D28: last hv {hv} vs qemu {qemu} {provenance} = {ratio:.2f}x"
                f" -> reported, no bar (D28 sets none for {workload})")
    op, bound = bar
    if op == "<=":
        meets = ratio <= bound
        return (f"  D28 bar: last hv {hv} vs qemu {qemu} {provenance} = {ratio:.2f}x"
                f" -> {'meets' if meets else 'FAILS'} D28 (boot <= {bound:.2f}x qemu)")
    meets = ratio >= bound
    return (f"  D28 bar: last hv {hv} vs qemu {qemu} {provenance} = {ratio:.2f}x"
            f" -> {'meets' if meets else 'FAILS'} D28 (throughput >= {bound:.2f}x qemu)")


def main(argv=None):
    args = parse_args(argv)
    arches = list(ARCHES) if args.arch == "all" else [a.strip() for a in args.arch.split(",")]
    for arch in arches:
        print_arch(arch, args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
