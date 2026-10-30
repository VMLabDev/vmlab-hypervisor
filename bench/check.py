#!/usr/bin/env python3
"""Fail when the latest run regressed beyond measured noise (D2).

For every (arch, backend, guest, vcpus, workload) with at least two history
entries, the latest median must not exceed the previous median by more than
the measured noise: the larger of the two runs' spreads, or 5 % of the
previous median. Comparisons are within one backend; the D1 hv-vs-qemu bar is
reported by `just bench-report`, not gated here.
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
    args = p.parse_args(argv)
    if args.guest == "all":
        args.guest = None
    return args


def compare(arch, args):
    """(regressions, notes): one line per config with two entries or fewer."""
    records = lib.read_history(arch)
    if args.backend != "all":
        records = [r for r in records if r["backend"] == args.backend]
    if args.guest:
        records = [r for r in records if r["guest"] == args.guest]
    grouped = {}
    for rec in records:
        grouped.setdefault(
            (rec["backend"], rec["guest"], rec["vcpus"], rec["workload"]), []
        ).append(rec)

    regressions, notes = [], []
    for key in sorted(grouped):
        entries = grouped[key]
        if len(entries) < 2:
            notes.append(
                f"{arch} {key[0]} {key[1]} vcpus={key[2]} {key[3]}: "
                f"only {len(entries)} entry, nothing to compare"
            )
            continue
        previous, latest = entries[-2], entries[-1]
        verdict, allowed = lib.regression_verdict(previous, latest)
        line = (
            f"{arch} {key[0]} {key[1]} vcpus={key[2]} {key[3]}: "
            f"{previous['median']}s -> {latest['median']}s "
            f"(noise <= {allowed}s) {verdict}"
        )
        if verdict == "worse":
            regressions.append(line)
        else:
            notes.append(line)
    return regressions, notes


def main(argv=None):
    args = parse_args(argv)
    arches = list(ARCHES) if args.arch == "all" else [a.strip() for a in args.arch.split(",")]
    regressions = []
    for arch in arches:
        arch_regressions, notes = compare(arch, args)
        for note in notes:
            print(f"  ok   {note}")
        regressions.extend(arch_regressions)
        for line in arch_regressions:
            print(f"  FAIL {line}", file=sys.stderr)
    if regressions:
        print(
            f"\nbench-check: {len(regressions)} regression(s) beyond measured noise",
            file=sys.stderr,
        )
        return 1
    print("\nbench-check: no regression beyond measured noise")
    return 0


if __name__ == "__main__":
    sys.exit(main())
