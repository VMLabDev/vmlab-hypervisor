#!/usr/bin/env python3
"""Compare stopped-state records from VMLAB_HV_SNAPSHOT_AUDIT."""

import argparse
import json
from pathlib import Path
import struct


CLOCK_MSRS = {0x10, 0x40000010, 0x40000020}


def normalize(key, value):
    if key == "kvmclock":
        # clock, realtime and host_tsc are sampled host timestamps.
        return value[8:16] + value[32:]
    if key == "tsc-offsets":
        # A common host-TSC translation must preserve all inter-vCPU offsets.
        offsets = value["offsets"]
        origin = offsets[0][1]
        return value["khz"], [(cpu, (offset - origin) % 2**64) for cpu, offset in offsets]
    if key == "pit":
        # Each channel's count_load_time is a host-monotonic timestamp.
        value = value.copy()
        for channel in range(3):
            value[channel * 24 + 16 : channel * 24 + 24] = [0] * 8
        return value
    if key.endswith("/lapic"):
        value = value.copy()
        value[0x390:0x394] = [0] * 4  # live timer current-count register
        return value
    if key.endswith("/msrs"):
        data = bytes(value)
        count = struct.unpack_from("<I", data)[0]
        assert len(data) == 4 + count * 12, "invalid MSR record"
        return {f"{index:#010x}": v for index, v in struct.iter_unpack("<IQ", data[4:]) if index not in CLOCK_MSRS}
    if key.endswith("/all-msrs"):
        return {index: v for index, v in value.items() if int(index, 16) not in CLOCK_MSRS}
    return value


def compare(directory):
    saved = {}
    failures = 0
    loads = 0
    for path in sorted(directory.glob("*.json")):
        pid, _, phase = path.stem.split("-", 2)
        state = json.loads(path.read_text())
        if phase == "save":
            saved[pid] = state
            continue
        assert pid in saved, f"no save record for {path}"
        expected = saved[pid]
        if phase == "ram-restored":
            keys = [key for key in expected if key.startswith("ram/")]
        elif phase == "load":
            loads += 1
            keys = sorted(set(expected) | set(state))
        else:
            raise ValueError(f"unknown audit phase {phase!r}")
        differences = []
        for key in keys:
            if key not in expected or key not in state:
                differences.append(f"{key}: missing field")
                continue
            before, after = normalize(key, expected[key]), normalize(key, state[key])
            if before == after:
                continue
            if isinstance(before, dict) and isinstance(after, dict):
                for field in sorted(set(before) | set(after)):
                    if before.get(field) != after.get(field):
                        differences.append(f"{key}/{field}: {before.get(field)!r} -> {after.get(field)!r}")
            else:
                differences.append(f"{key}: differs")
        failures += len(differences)
        print(f"{path.name}: {len(differences)} differences")
        for difference in differences:
            print(f"  {difference}")
    assert loads, "no completed load records"
    print(f"Compared {loads} loads; {failures} non-clock differences")
    return bool(failures)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    raise SystemExit(compare(parser.parse_args().directory))
