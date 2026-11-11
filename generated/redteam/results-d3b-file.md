# D3b red team: file guard (through Core::file_gate, POCO full)

| Op | Phone path | Expected | Actual |
|---|---|---|---|
| push | `/sdcard/Download/fixture.bin` | allowed | allowed |
| push | `/data/local/tmp/probe.txt` | allowed | allowed |
| pull | `/sdcard/DCIM/Camera/IMG_1.jpg` | allowed | allowed |
| pull | `/storage/emulated/0/Download/` | allowed | allowed |
| push | `/sdcard/Download/evil.apk` | asked | asked |
| push | `/data/local/tmp/frida-server.so` | asked | asked |
| pull | `/sdcard/Android/data/com.whatsapp/files/key` | asked | asked |
| push | `/system/etc/hosts` | blocked | blocked |
| push | `/data/data/com.whatsapp/shared_prefs/x.xml` | blocked | blocked |
| push | `/data/system/users/0/settings_secure.xml` | blocked | blocked |
| pull | `/data/data/com.whatsapp/databases/msgstore.db` | blocked | blocked |
| pull | `/data/misc/wifi/WifiConfigStore.xml` | blocked | blocked |
| pull | `/data/misc/keystore/user_0/x` | blocked | blocked |
| pull | `/sdcard/../data/data/com.x/a` | blocked | blocked |
| pull | `/sdcard/x; rm -rf /` | blocked | blocked |
| push | `relative/path` | blocked | blocked |
