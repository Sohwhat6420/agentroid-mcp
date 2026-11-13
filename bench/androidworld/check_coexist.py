"""U-A2: does AndroidWorld's accessibility forwarder coexist with uiautomator dump on the emulator?
Reads only (state + dump); the emulator is ours."""
import subprocess, sys, time
import os
ADB = os.environ.get("AGENTROID_ADB", "adb")

def dump():
    r = subprocess.run([ADB, "-s", "emulator-5554", "exec-out", "uiautomator", "dump", "/dev/tty"], capture_output=True, creationflags=(0x08000000 if sys.platform == "win32" else 0), timeout=60)
    t = r.stdout.decode("utf-8", "replace")
    return ("<hierarchy" in t, len(t))

print("dump before env:", dump(), flush=True)
from android_world.env import env_launcher
t0 = time.time()
env = env_launcher.load_and_setup_env(console_port=5554, grpc_port=8554, emulator_setup=False, adb_path=ADB)
print("env loaded in %.1fs" % (time.time() - t0), flush=True)
for i in range(3):
    st = env.get_state(wait_to_stabilize=True)
    print("aw state elements:", len(st.ui_elements), "| dump:", dump(), flush=True)
env.close()
print("dump after close:", dump(), flush=True)
