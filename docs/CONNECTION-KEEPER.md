# The connection keeper

> Moved here from the README on 14 Nov 2026, unchanged except for links, so the README can lead with the context budget.

`android/keeper` is an Android module for a phone you leave connected over Wireless debugging. A standalone 25 KB build of the same keeper, with its own build and setup scripts, is in [apps/keeper](../apps/keeper). adb over Wi-Fi switches itself off when the Wi-Fi drops, the phone reboots or the app that holds it is killed, and the next command then fails until someone taps the Wireless debugging tile. The keeper checks on a 15-minute alarm (Doze stretches that to about 20 minutes) and whenever the network is lost or comes back, and switches Wireless debugging back on. It also registers for the boot broadcast that runs before the first unlock, so a phone that restarts overnight comes back without a tap.

What it needs: a one-time `adb shell pm grant` of `WRITE_SECURE_SETTINGS` (only adb can give it), Wireless debugging switched on once, and, on Xiaomi HyperOS, Autostart and battery "No restrictions". Android asks "Allow wireless debugging on this network?" once for each Wi-Fi access point (BSSID); tick *Always allow* for each one you use, because the keeper cannot answer that prompt.

What was measured, all on one phone (Xiaomi POCO X6 Pro, Android 15, HyperOS 2.0) over wireless adb, with no USB cable at any point:

- *Wi-Fi switched off for 10 s, screen off, phone unplugged:* the adb link was back after 12, 12 and 13 s (3 of 3 rounds), with no tap. These used a fixed address (`adb tcpip 5555`); a recovery of the TLS port alone was not timed.
- *Process crash (`am crash`):* the keeper was running again after about 17 minutes, through its alarm.
- *Cold boot behind the lock screen* (the phone had shut down on a flat battery, and was restarted): Wireless debugging was switched on before the first unlock, and the laptop reconnected about three minutes later without a tap. This happened once, by accident, and was not repeated.
- *Not tested:* a real router outage, and a second access point (which needs its one prompt tapped).

Limits: on this phone the Wi-Fi itself sometimes took 4 to 30 minutes to rejoin after being switched off with the screen off and the phone unplugged, and the adb link came back about 40 s after the Wi-Fi did. The keeper cannot shorten that. A later version wakes the screen for a few seconds while the network is gone to speed the rejoin; it is installed but has not been seen to fire in a test, so no claim is made for it. A flat battery stops everything. The 5555 address does not survive a reboot (the keeper restores Wireless debugging, and the TLS entry reconnects, but `adb tcpip 5555` must be run again).
