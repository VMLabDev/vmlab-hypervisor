#!/usr/bin/env python3
"""Build pinned upstream iPXE PCI ROMs without QEMU firmware inputs."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
OUTPUT = ROOT / "target/ipxe"


def run(*args, **kwargs):
    return subprocess.run(args, check=True, **kwargs)


def main():
    pin = json.loads((HERE / "upstream.json").read_text())
    source = OUTPUT / "upstream"
    OUTPUT.mkdir(parents=True, exist_ok=True)
    if not source.exists():
        run("git", "clone", "--no-checkout", pin["url"], str(source))
        run("git", "-C", str(source), "checkout", "--detach", pin["commit"])
    actual = subprocess.check_output(
        ["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    if actual != pin["commit"]:
        raise SystemExit(f"{source}: expected commit {pin['commit']}, got {actual}")
    if subprocess.check_output(["git", "-C", str(source), "diff", "HEAD", "--"]):
        raise SystemExit(f"{source}: tracked source changes; refusing an unpinned build")
    untracked = subprocess.check_output(
        ["git", "-C", str(source), "ls-files", "--others", "--exclude-standard"], text=True)
    if untracked.strip():
        raise SystemExit(f"{source}: untracked source files; refusing an unpinned build")
    src = source / "src"
    local = src / "config/local"
    local.mkdir(exist_ok=True)
    for config in sorted((HERE / "config").glob("*.h")):
        shutil.copyfile(config, local / config.name)
    env = dict(os.environ, SOURCE_DATE_EPOCH=pin["source_date_epoch"], LC_ALL="C", TZ="UTC")
    for target in ("bin/1af41000.rom", "bin-x86_64-efi/1af41000.efirom"):
        run("make", "-C", str(src), f"-j{min(os.cpu_count() or 1, 8)}",
            "BUILD_SERIAL=0", "NO_WERROR=1", target, env=env)
        shutil.copyfile(src / target, OUTPUT / Path(target).name)
    with (OUTPUT / "virtio-net.rom").open("wb") as output:
        run("perl", str(src / "util/catrom.pl"), str(OUTPUT / "1af41000.rom"),
            str(OUTPUT / "1af41000.efirom"), stdout=output, env=env)
    # Ship the complete corresponding source, including the build configuration.
    with (OUTPUT / "ipxe-source.tar").open("wb") as output:
        run("git", "-C", str(source), "archive", "--format=tar", "--prefix=ipxe/",
            pin["commit"], stdout=output)
    with tarfile.open(OUTPUT / "ipxe-source.tar", "a") as archive:
        for path in (HERE / "upstream.json", HERE / "build.py", *sorted((HERE / "config").glob("*.h"))):
            info = archive.gettarinfo(str(path), "ipxe/vmlab-build/" + path.relative_to(HERE).as_posix())
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mtime = int(pin["source_date_epoch"])
            info.mode = 0o644
            with path.open("rb") as data:
                archive.addfile(info, data)
    names = ("1af41000.rom", "1af41000.efirom", "virtio-net.rom", "ipxe-source.tar")
    (OUTPUT / "SHA256SUMS").write_text("".join(
        f"{hashlib.sha256((OUTPUT / name).read_bytes()).hexdigest()}  {name}\n"
        for name in names))
    print((OUTPUT / "SHA256SUMS").read_text(), end="")


if __name__ == "__main__":
    main()
