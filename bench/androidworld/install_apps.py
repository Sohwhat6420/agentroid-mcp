"""Install AndroidWorld's benchmark apps on the EMULATOR from files already downloaded to the folder named by AW_APKS.

    python -B install_apps.py [--only markor,simple_sms_messenger,...]

Copies the downloaded files into android_world's own cache folder (so its setup never downloads anything itself), then
runs android_world's `setup_apps`, which installs each APK (adb install on the emulator's console port only) and
performs the app's own first-run setup. Needs the emulator up (U-A2-RESULTS.md). Never touches a phone: make_env
talks only to emulator-5554.
"""

from __future__ import annotations

import argparse
import os
import shutil
import sys
import time
from pathlib import Path

import run_pilot as rp

APKS = Path(r"the folder AW_APKS names")


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--only", help="comma-separated AndroidWorld app_names (default: all)")
    args = ap.parse_args(argv)

    from android_world.env.setup_device import apps, setup  # type: ignore[import-not-found]
    from android_world.utils import file_utils  # type: ignore[import-not-found]

    cache = Path(file_utils.get_local_tmp_directory()) / "android_world" / "app_data"
    cache.mkdir(parents=True, exist_ok=True)
    files = sorted(p for p in APKS.iterdir() if p.is_file())
    if not files:
        print(f"no files in {APKS}", file=sys.stderr)
        return 2
    for p in files:
        if not (cache / p.name).is_file() or (cache / p.name).stat().st_size != p.stat().st_size:
            shutil.copy2(p, cache / p.name)
    print(f"{len(files)} files in the android_world cache {cache}")

    app_list = tuple(setup._APPS)
    if args.only:
        want = {w.strip() for w in args.only.split(",") if w.strip()}
        app_list = tuple(a for a in app_list if a.app_name in want)
        missing = want - {a.app_name for a in app_list}
        if missing:
            print(f"unknown app names: {sorted(missing)}", file=sys.stderr)
            return 2
    env, _ = rp.make_env()
    try:
        t0 = time.monotonic()
        setup.setup_apps(env, app_list)
        print(f"set up {len(app_list)} apps in {time.monotonic() - t0:.0f} s")
        installed = setup.get_installed_packages(env)
        for a in app_list:
            if a.apk_names:
                pkg = a.package_name()
                print(f"  {a.app_name:24} {'installed' if pkg in installed else 'MISSING'}  {pkg}")
    finally:
        env.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
