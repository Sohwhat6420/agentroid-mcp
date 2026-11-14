# Agentroid Bank Guard

**A small Android app that switches USB and Wireless debugging off while a banking app is open, then back on when
you leave it.** Singapore banking apps (DBS digibank, PayLah!, POSB, OCBC, UOB TMRW) and Singpass refuse to run while
debugging is on. A phone that an agent drives over adb otherwise has to be unplugged from its laptop by hand every
time you want to pay someone.

It is part of **Agentroid** (the rest of this repository). It stands alone and also works on a phone that has no
other Agentroid app.

## How it works

- **Which app is in front:** read from Usage access (`UsageStatsManager`), polled about every 400 ms while the screen
  is on and not at all while it is off. It has **no AccessibilityService, on purpose.** OCBC, DBS and UOB block their
  apps when an app from outside an official store has Accessibility on, and the guard must not be that app.
- **The switch:** `Settings.Global` `adb_enabled` and `adb_wifi_enabled`, written with `WRITE_SECURE_SETTINGS`,
  which is granted once over adb with `pm grant`. Those two keys are the only settings it ever writes.
- **Putting things back:**
  - It puts back only what it switched off itself. If debugging was already off, it stays off.
  - It waits until you have been out of the bank for 3 s. The shade, the keyboard, biometric prompts and the
    permission dialog do not count as leaving.
  - Wireless debugging comes back only on the same Wi-Fi network it was switched off on. On any other network
    Android would ask you to allow that network, so the guard leaves it off and gives you a notification to tap.
  - With the screen off, it holds for 5 minutes and then puts debugging back.
  - After a reboot, it puts debugging back at once.
- **The race, and the shortcut:**
  - A bank may check debugging in the first moments after it starts, before any poll can see it.
  - Long-press an app in the guard's list for a home-screen shortcut, "UOB TMRW (safe)". It switches debugging off
    first and then opens the bank, so the bank's first check already sees it off.
  - When the poll catches a bank you opened directly, the guard switches debugging off and tells you to close and
    reopen the bank if it showed a warning.
- **With the Agentroid keeper:** the keeper (`apps/keeper`) keeps Wireless debugging on. It now asks this app's
  read-only `content://dev.agentroid.bankguard.hold` first, and stands down while a bank is open. The answer says only
  "held" or "not held", which any app can already infer from `adb_enabled`. A hold the guard stopped refreshing goes
  void after 10 minutes.
- **No INTERNET permission:** it never sends anything anywhere.

## Set up

```bash
./build.sh                                   # out/bankguard.apk, no Gradle (JDK 17 + Android SDK 36)
ADB=<path to adb> ./setup-bankguard.sh <SERIAL>
```

The setup script does the following:

- installs the app;
- grants `WRITE_SECURE_SETTINGS`, Usage access and notifications;
- exempts the app from battery optimisation;
- lists the bank-like apps it finds, so you can check the list covers yours;
- starts the guard.

On Xiaomi/HyperOS, `pm grant` needs **USB debugging (Security settings)** on. Also set the app's battery mode to
**No restrictions** and turn Autostart on. On the phone, open **Agentroid Bank Guard** to tick or untick apps, and
long-press one for its safe shortcut. From a computer:

```bash
adb -s <SERIAL> shell am broadcast -n dev.agentroid.bankguard/.ConfigReceiver -a dev.agentroid.bankguard.SET --es banks "com.uob.mighty.app,sg.ndi.sp"
adb -s <SERIAL> shell dumpsys activity service dev.agentroid.bankguard/.GuardService   # state, timings, recent events
```

Only the adb shell can send that broadcast, because the receiver needs `android.permission.DUMP`.

**Test without a bank app:**

1. Put `dev.agentroid.bankguard/dev.agentroid.bankguard.TestBank` on the list.
2. Run `adb shell am start -n dev.agentroid.bankguard/.TestBank --el close_after_ms 15000`.

adb drops while the test bank is open. The test bank closes itself, and adb comes back.

**Home Wi-Fi mode (optional).** With it on, Wireless debugging is on only while the phone is on your home network,
switches off the moment the phone leaves it, and also switches off while a bank app is open at home. USB debugging
keeps the bank rule.

1. At home, list the access points of your network: `adb shell cmd wifi list-scan-results`. A mesh has several, and
   a 3 to 5 octet prefix ending in `:*` covers nodes and bands you didn't see.
2. Run the setup with `BANKGUARD_HOME_SSID="<name>" BANKGUARD_HOME_BSSIDS="<bssid>,<prefix>:*"`. It grants location
   access (Android shows a network's name to a background app only with it) and sets the network.
3. Check: `on_home_wifi=true` in the service's dump.

- The name alone is never enough: a copied network name on another access point is not trusted.
- If the name can't be read (location off, or no location access), the network counts as not home, so Wireless
  debugging stays off.
- It runs on the system's network callback, with no polling and no alarm. Clear it with `--es home_ssid "''"`.

## Tests

```bash
mkdir -p out/test && javac -d out/test src/dev/agentroid/bankguard/GuardLogic.java test/GuardLogicTest.java && java -cp out/test GuardLogicTest
```

The decisions live in `GuardLogic.java`, which has no Android imports.

## Limits

- **Developer options stays on.** The guard switches off debugging, not Developer options. The consumer bank apps
  above document only the debugging checks. Singpass's help also asks for Developer options off, and banks may
  tighten their checks. That would need one more key, with care, because switching Developer options off resets
  some of its settings.
- **Android 17:** the write needs testing on each phone. The setup script checks that the grant took. A device
  policy (`DISALLOW_DEBUGGING_FEATURES`) refuses the write.
- **While a bank is open, the laptop cannot reach the phone.** That is the point. Agentroid sees the phone drop off
  and come back.
- **A bank that keeps checking after you leave it** (in the background) could see debugging come back. No Singapore
  bank documents doing that.
