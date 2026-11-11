"""Patch android_env (installed in the AndroidWorld venv) so its accessibility-forwarder APK installs on Windows.

android_env writes the APK to a NamedTemporaryFile that is still open (and unflushed) when adb reads it, which
fails on Windows ("failed to open ...apk: No such file"). This writes, closes, installs, then deletes.
Idempotent. Run with the venv's python:  python -B patch_android_env_windows.py

Attribution: the OLD text below is a few lines of upstream android_env code (google-deepmind/android_env,
Apache License 2.0, https://github.com/google-deepmind/android_env), quoted only so the patch can find them.
The NEW text is a modification of those lines for Windows. See NOTICE.
"""

import sys
from pathlib import Path

import android_env

OLD = """        with tempfile.NamedTemporaryFile(**kwargs) as f:
          fpath = f.name
          f.write(install_apk.blob.contents)

          response, _ = self._execute_command(
              ['install', '-r', '-t', '-g', fpath], timeout=timeout
          )
"""
NEW = """        import os as _os
        with tempfile.NamedTemporaryFile(suffix='.apk', delete=False) as f:
          fpath = f.name
          f.write(install_apk.blob.contents)
        try:
          response, _ = self._execute_command(
              ['install', '-r', '-t', '-g', fpath], timeout=timeout
          )
        finally:
          _os.unlink(fpath)
"""

path = Path(android_env.__file__).parent / "components" / "adb_call_parser.py"
text = path.read_text(encoding="utf-8").replace("\r\n", "\n")
if "suffix='.apk', delete=False) as f:" in text:
    print("already patched")
elif OLD in text:
    path.write_text(text.replace(OLD, NEW), encoding="utf-8", newline="")
    print("patched", path)
else:
    sys.exit(f"{path}: the code it patches is not there (android_env changed?)")
