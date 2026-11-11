"""The small-RAM fallback AVD, for a start when memory is between 38 and 42 GB committed.

    python -B small_avd.py status                 what exists, and which AVD start-emulator.ps1 will use
    python -B small_avd.py create [--ram 2560]    copy AndroidWorldAvd to AndroidWorldSmallAvd with a smaller hw.ramSize (about 5 GB of disk)
    python -B small_avd.py use-small              point start-emulator.ps1 at the small AVD (writes results/avd.name)
    python -B small_avd.py use-standard           point it back at AndroidWorldAvd (removes results/avd.name)

Nothing here boots an emulator, and `create` never touches the standard AVD. The deviation from the standard AVD (less RAM)
must be stated next to the benchmark score in the README if the small one is used for the scored run.
"""

from __future__ import annotations

import argparse
import os
import re
import shutil
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
AVD_HOME = Path(os.environ.get("ANDROID_AVD_HOME") or Path.home() / ".android" / "avd")
STANDARD = "AndroidWorldAvd"
SMALL = "AndroidWorldSmallAvd"
NAME_FILE = HERE / "results" / "avd.name"
SKIP = {"snapshots", "tmpAdbCmds", "hardware-qemu.ini.lock", "multiinstance.lock", "bootcompleted.ini", "read-snapshot.txt", "quickbootChoice.ini"}


def set_key(text: str, key: str, value: str) -> str:
    pat = re.compile(rf"^{re.escape(key)}=.*$", re.M)
    return pat.sub(f"{key}={value}", text) if pat.search(text) else text.rstrip("\n") + f"\n{key}={value}\n"


def create(ram: int) -> None:
    src = AVD_HOME / f"{STANDARD}.avd"
    dst = AVD_HOME / f"{SMALL}.avd"
    if not src.is_dir():
        raise SystemExit(f"{src} not found")
    if dst.exists():
        raise SystemExit(f"{dst} already exists; remove it first (bin-dump it, never delete)")
    shutil.copytree(src, dst, ignore=lambda d, names: [n for n in names if n in SKIP])
    cfg = dst / "config.ini"
    t = cfg.read_text(encoding="utf-8")
    t = set_key(t, "hw.ramSize", str(ram))
    t = set_key(t, "AvdId", SMALL)
    t = set_key(t, "avd.ini.displayname", SMALL)
    cfg.write_text(t, encoding="utf-8")
    (AVD_HOME / f"{SMALL}.ini").write_text(
        f"avd.ini.encoding=UTF-8\npath={dst}\npath.rel=avd\\{SMALL}.avd\ntarget=android-33\n", encoding="utf-8")
    print(f"created {dst} with hw.ramSize={ram}")


def current() -> str:
    return NAME_FILE.read_text(encoding="utf-8").strip() if NAME_FILE.exists() else STANDARD


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["status", "create", "use-small", "use-standard"])
    ap.add_argument("--ram", type=int, default=2560)
    a = ap.parse_args(argv)
    if a.cmd == "create":
        create(a.ram)
    elif a.cmd == "use-small":
        if not (AVD_HOME / f"{SMALL}.avd").is_dir():
            raise SystemExit("create the small AVD first")
        NAME_FILE.parent.mkdir(parents=True, exist_ok=True)
        NAME_FILE.write_text(SMALL, encoding="utf-8")
        print("start-emulator.ps1 will use", SMALL)
    elif a.cmd == "use-standard":
        NAME_FILE.unlink(missing_ok=True)
        print("start-emulator.ps1 will use", STANDARD)
    else:
        for n in (STANDARD, SMALL):
            d = AVD_HOME / f"{n}.avd"
            ram = re.search(r"^hw.ramSize=(.*)$", (d / "config.ini").read_text(encoding="utf-8"), re.M) if d.is_dir() else None
            print(f"{n}: {'exists, hw.ramSize=' + ram.group(1) if ram else 'absent'}")
        print("will start:", current())
    return 0


if __name__ == "__main__":
    sys.exit(main())
