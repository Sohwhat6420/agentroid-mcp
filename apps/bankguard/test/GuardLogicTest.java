import dev.agentroid.bankguard.GuardLogic;
import dev.agentroid.bankguard.GuardLogic.Action;
import dev.agentroid.bankguard.GuardLogic.Seen;
import dev.agentroid.bankguard.GuardLogic.State;
import dev.agentroid.bankguard.GuardLogic.Visible;

import java.util.Set;

/** Plain-JVM tests for GuardLogic: no Android, no phone. Exit code 0 when all pass. */
public final class GuardLogicTest {
    static int run, failed;
    static final String UOB = "com.uob.mighty.app", BANK = UOB + "/com.uob.Main", HOME = "com.miui.home/.Launcher";
    static final String SMS = "com.google.android.apps.messaging/.ui.ConversationActivity";
    static final Set<String> BANKS = GuardLogic.parseList("com.uob.mighty.app, sg.ndi.sp,dev.agentroid.bankguard/dev.agentroid.bankguard.TestBank");
    static final long NET = 4242;
    static final long HG = GuardLogic.HOME_GRACE_MS, OG = GuardLogic.OTHER_APP_GRACE_MS;

    static void check(String name, boolean ok) {
        run++;
        if (!ok) { failed++; System.out.println("FAIL " + name); } else System.out.println("ok   " + name);
    }

    /** The bank in front, unlocked, on the home Wi-Fi. */
    static Seen bank(long t, boolean usb, boolean wifi) { return new Seen(BANK, UOB, t, usb, wifi, true, NET, true, false); }
    /** The home screen in front, no bank visible. */
    static Seen home(long t, boolean usb, boolean wifi) { return new Seen(HOME, null, t, usb, wifi, true, NET, true, true); }
    /** Another app in front (Messages), no bank visible. */
    static Seen other(long t, boolean usb, boolean wifi) { return new Seen(SMS, null, t, usb, wifi, true, NET, true, false); }

    static boolean is(Integer v, int want) { return v != null && v == want; }

    public static void main(String[] a) {
        // 1. a bank in front with both on: hold, both to 0, remember both
        State st = new State();
        Action x = GuardLogic.tick(st, bank(1_000, true, true));
        check("hold writes usb=0 and wifi=0", "hold".equals(x.event) && is(x.usb, 0) && is(x.wifi, 0) && x.switchesOff());
        check("hold remembers both and the network", st.held && st.restoreUsb && st.restoreWifi && st.network == NET);
        check("held package is the package", UOB.equals(st.heldPkg));

        // 2. something (the keeper, the owner) switches wifi back on while held: off again, restore set unchanged
        x = GuardLogic.tick(st, bank(1_400, false, true));
        check("re-assert wifi=0 only", "reassert".equals(x.event) && x.usb == null && is(x.wifi, 0));
        x = GuardLogic.tick(st, bank(1_800, false, false));
        check("nothing to do while held and off", x.event == null && !x.writes());
        State st2 = new State();
        GuardLogic.tick(st2, bank(0, true, false)); // only USB was on at the hold
        x = GuardLogic.tick(st2, bank(500, false, true)); // Wireless switched on during the hold
        check("switched on during the hold: re-asserted", "reassert".equals(x.event) && is(x.wifi, 0));
        x = GuardLogic.tick(st2, home(10_000, false, false));
        check("...and not restored, because the guard did not find it on", is(x.usb, 1) && x.wifi == null && !st2.wifiLeftOff);

        // 3. leave to the home screen: nothing inside HOME_GRACE, release after it, same network
        x = GuardLogic.tick(st, home(1_800 + HG - 1, false, false));
        check("no release inside the home grace", x.event == null && st.held);
        x = GuardLogic.tick(st, home(1_800 + HG, false, false));
        check("release puts both back", "release".equals(x.event) && is(x.usb, 1) && is(x.wifi, 1) && !st.held);
        x = GuardLogic.tick(st, home(9_000, true, true));
        check("after release nothing more", x.event == null);

        // 4. leave for another app (an SMS code): the hold stays for OTHER_APP_GRACE, then releases
        st = new State();
        GuardLogic.tick(st, bank(0, true, true));
        x = GuardLogic.tick(st, other(OG - 1, false, false));
        check("another app: held inside the long grace", x.event == null && st.held);
        x = GuardLogic.tick(st, bank(OG - 1, false, false));
        check("back in the bank: still held, no writes", x.event == null && st.held);
        x = GuardLogic.tick(st, other(OG - 1 + OG, false, false));
        check("another app: released after the long grace", "release".equals(x.event) && is(x.usb, 1));

        // 5. a prompt over the bank (systemui, a HyperOS permission dialog) or an unknown front app keeps the hold
        st = new State();
        GuardLogic.tick(st, bank(0, true, true));
        x = GuardLogic.tick(st, new Seen("com.lbe.security.miui/.Perm", null, 600_000, false, false, true, NET, true, false));
        check("pass-through keeps the hold", x.event == null && st.held);
        x = GuardLogic.tick(st, new Seen(null, null, 1_200_000, false, false, true, NET, true, false));
        check("unknown foreground keeps the hold", x.event == null && st.held);
        x = GuardLogic.tick(st, home(1_200_000 + HG - 1, false, false));
        check("after a long prompt, home still gets its grace", x.event == null && st.held);

        // 6. split screen: the bank is visible in one half, another app resumed in the other
        st = new State();
        GuardLogic.tick(st, bank(0, true, true));
        x = GuardLogic.tick(st, new Seen(SMS, UOB, 10 * OG, false, false, true, NET, true, false));
        check("split screen: a visible bank holds whatever is on top", x.event == null && st.held);

        // 7. debugging already off: hold writes nothing and release writes nothing
        st = new State();
        x = GuardLogic.tick(st, bank(0, false, false));
        check("hold with both off writes nothing", "hold".equals(x.event) && !x.writes() && !x.switchesOff());
        x = GuardLogic.tick(st, home(10_000, false, false));
        check("never switches on what it did not switch off", "release".equals(x.event) && !x.writes());

        // 8. the Wi-Fi network changed, or is gone: wireless debugging stays off and the owner is told
        st = new State();
        GuardLogic.tick(st, bank(0, true, true));
        x = GuardLogic.tick(st, new Seen(HOME, null, 10_000, false, false, true, NET + 1, true, true));
        check("other network: usb back, wifi left off", is(x.usb, 1) && x.wifi == null
                && "release_wifi_left_off".equals(x.event) && st.wifiLeftOff);
        st = new State();
        GuardLogic.tick(st, bank(0, true, true));
        x = GuardLogic.tick(st, new Seen(HOME, null, 10_000, false, false, false, 0, true, true));
        check("off Wi-Fi: wifi left off", x.wifi == null && st.wifiLeftOff);
        st = new State();
        GuardLogic.tick(st, new Seen(BANK, UOB, 0, true, true, false, 0, true, false)); // held on mobile data
        x = GuardLogic.tick(st, home(10_000, false, false));
        check("held off Wi-Fi then on Wi-Fi: still left off", x.wifi == null && st.wifiLeftOff);
        st = new State();
        GuardLogic.tick(st, bank(0, true, true));
        GuardLogic.tick(st, new Seen(null, null, 1_000, false, false, true, NET + 7, false, false)); // screen off, network changed
        x = GuardLogic.tick(st, new Seen(null, null, 1_000 + GuardLogic.SCREEN_OFF_RELEASE_MS, false, false, true, NET + 7, false, false));
        check("screen-off release after a network change: wifi left off", "release_screen_off_wifi_left_off".equals(x.event)
                && is(x.usb, 1) && x.wifi == null);

        // 9. screen off, or locked, with the bank open: hold for SCREEN_OFF_RELEASE_MS, then release
        st = new State();
        GuardLogic.tick(st, bank(0, true, true));
        x = GuardLogic.tick(st, new Seen(BANK, UOB, 1_000, false, false, true, NET, false, false));
        check("screen off: still held", x.event == null && st.held && st.screenOffAt == 1_000);
        x = GuardLogic.tick(st, new Seen(BANK, UOB, 1_000 + GuardLogic.SCREEN_OFF_RELEASE_MS - 1, false, false, true, NET, false, false));
        check("screen off: no release before the limit", x.event == null && st.held);
        x = GuardLogic.tick(st, new Seen(BANK, UOB, 1_000 + GuardLogic.SCREEN_OFF_RELEASE_MS, false, false, true, NET, false, false));
        check("screen off: release at the limit", "release_screen_off".equals(x.event) && is(x.usb, 1) && is(x.wifi, 1));
        // the review's finding 2: a wake with the bank still last (behind the keyguard) must not hold again
        x = GuardLogic.tick(st, new Seen(BANK, UOB, 2_000 + GuardLogic.SCREEN_OFF_RELEASE_MS, true, true, true, NET, false, false));
        check("locked wake with the bank last: no new hold", x.event == null && !st.held);
        x = GuardLogic.tick(st, bank(3_000 + GuardLogic.SCREEN_OFF_RELEASE_MS, true, true));
        check("unlocked into the bank: holds again", "hold".equals(x.event) && x.switchesOff());
        st = new State();
        GuardLogic.tick(st, bank(0, true, true));
        GuardLogic.tick(st, new Seen(BANK, UOB, 1_000, false, false, true, NET, false, false));
        x = GuardLogic.tick(st, bank(2_000, false, false));
        check("screen back on in the bank: the off timer resets", x.event == null && st.held && st.screenOffAt == 0);

        // 10. one bank to another stays held
        st = new State();
        GuardLogic.tick(st, bank(0, true, true));
        x = GuardLogic.tick(st, new Seen("sg.ndi.sp/.Main", "sg.ndi.sp", 500, false, false, true, NET, true, false));
        check("bank to bank: still held, package follows", x.event == null && st.held && "sg.ndi.sp".equals(st.heldPkg));

        // 11. the "(safe)" shortcut: held before the bank shows, kept through a slow start, then normal rules
        st = new State();
        x = GuardLogic.holdForLaunch(st, new Seen(UOB, UOB, 0, true, true, true, NET, true, false));
        check("shortcut: holds at once", "hold".equals(x.event) && x.switchesOff() && st.launchUntil == GuardLogic.LAUNCH_MS);
        x = GuardLogic.tick(st, home(GuardLogic.LAUNCH_MS - 1, false, false));
        check("shortcut: the launcher still on top inside LAUNCH_MS keeps the hold", x.event == null && st.held);
        x = GuardLogic.tick(st, bank(GuardLogic.LAUNCH_MS - 1, false, false));
        check("shortcut: the bank arrives, the launch window ends", x.event == null && st.launchUntil == 0);
        st = new State();
        GuardLogic.holdForLaunch(st, new Seen(UOB, UOB, 0, true, true, true, NET, true, false));
        x = GuardLogic.tick(st, home(GuardLogic.LAUNCH_MS + HG, false, false));
        check("shortcut: a bank that never shows is released after the window", "release".equals(x.event) && is(x.usb, 1));

        // 12. list entries: a package matches any activity; an activity entry matches only that activity
        String tb = "dev.agentroid.bankguard/dev.agentroid.bankguard.TestBank";
        check("activity entry matches", GuardLogic.isBank(BANKS, tb));
        check("other activity of that app is not a bank", !GuardLogic.isBank(BANKS, "dev.agentroid.bankguard/dev.agentroid.bankguard.MainActivity"));
        check("package entry matches any activity", GuardLogic.isBank(BANKS, "com.uob.mighty.app/x.Y"));
        check("null is not a bank", !GuardLogic.isBank(BANKS, null));
        Set<String> bad = GuardLogic.parseList("a, ../etc, com.x;rm, com..x, com.ok.app, com.ok.app/..Evil, com.ok.app/a.B$C");
        check("invalid entries dropped", bad.size() == 2 && bad.contains("com.ok.app") && bad.contains("com.ok.app/a.B$C"));

        // 13. release by hand, and release when nothing is held
        st = new State();
        GuardLogic.tick(st, bank(0, true, true));
        x = GuardLogic.release(st, bank(100, false, false), "release_owner");
        check("owner's release puts both back", "release_owner".equals(x.event) && is(x.usb, 1) && is(x.wifi, 1) && !st.held);
        x = GuardLogic.release(st, bank(200, false, false), "release_owner");
        check("release with nothing held does nothing", x.event == null && !x.writes());

        // 14. boot: only a hold from an earlier boot is released, and never Wireless debugging
        st = new State();
        GuardLogic.tick(st, bank(0, true, true));
        check("boot: a hold from this boot stays", !GuardLogic.bootShouldRelease(st, false));
        check("boot: a hold from an earlier boot is released", GuardLogic.bootShouldRelease(st, true));
        st.forgetBootTimes();
        x = GuardLogic.release(st, new Seen(null, null, 50, false, false, false, 0, true, false), "release_boot");
        check("boot release: usb back, wifi left off", is(x.usb, 1) && x.wifi == null && st.wifiLeftOff);
        st = new State();
        GuardLogic.tick(st, bank(0, true, true));
        st.forgetBootTimes();
        x = GuardLogic.release(st, new Seen(null, null, 50, false, false, true, NET, true, false), "release_boot");
        check("boot release: a handle from the last boot never matches", x.wifi == null && st.wifiLeftOff);

        // 15. time is elapsedRealtime: a release decision never goes negative or waits on a wall clock
        st = new State();
        GuardLogic.tick(st, bank(100_000, true, true));
        x = GuardLogic.tick(st, home(100_000 + HG, false, false));
        check("monotonic: release exactly at the grace", "release".equals(x.event));

        // 16. what the keeper is told
        st = new State();
        check("no hold, nothing left off: keeper goes ahead", !GuardLogic.keeperShouldWait(st, 0, 0, true));
        GuardLogic.tick(st, bank(1_000, false, true));
        check("live hold that switched wifi off: keeper waits", GuardLogic.keeperShouldWait(st, 1_000, 1_000 + GuardLogic.HOLD_STALE_MS - 1, true));
        check("stale hold: keeper goes ahead", !GuardLogic.keeperShouldWait(st, 1_000, 1_000 + GuardLogic.HOLD_STALE_MS, true));
        check("hold from an earlier boot: not live", !GuardLogic.keeperShouldWait(st, 1_000, 1_000, false));
        check("refresh in the future: not live", !GuardLogic.keeperShouldWait(st, 5_000, 1_000, true));
        st = new State();
        GuardLogic.tick(st, bank(0, true, false)); // wifi was already off: says nothing about the bank
        check("hold with wifi already off: keeper is not told (no leak)", !GuardLogic.keeperShouldWait(st, 0, 0, true));
        st = new State();
        st.wifiLeftOff = true;
        check("wifi left off after a release: keeper waits", GuardLogic.keeperShouldWait(st, 0, 0, true));

        // 17. the visible set from UsageEvents
        Visible v = new Visible();
        v.event(GuardLogic.EV_RESUMED, UOB, "com.uob.Main");
        check("visible: a resumed bank", UOB.equals(v.bank(BANKS)) && BANK.equals(v.top));
        v.event(GuardLogic.EV_PAUSED, UOB, "com.uob.Main");
        v.event(GuardLogic.EV_RESUMED, "com.google.android.apps.messaging", ".ui.ConversationActivity");
        check("visible: paused under split screen still counts", UOB.equals(v.bank(BANKS)) && SMS.equals(v.top));
        v.event(GuardLogic.EV_STOPPED, UOB, "com.uob.Main");
        check("visible: stopped bank is gone", v.bank(BANKS) == null && v.size() == 1);
        v.event(GuardLogic.EV_STOPPED, UOB, "com.uob.Main");
        v.event(GuardLogic.EV_RESUMED, "com.google.android.apps.messaging", ".ui.ConversationActivity");
        check("visible: replayed events change nothing", v.bank(BANKS) == null && v.size() == 1);
        v.event(GuardLogic.EV_RESUMED, "dev.agentroid.bankguard", "dev.agentroid.bankguard.MainActivity");
        check("visible: another activity of an activity-listed app is not a bank", v.bank(BANKS) == null);
        v.event(GuardLogic.EV_RESUMED, "dev.agentroid.bankguard", "dev.agentroid.bankguard.TestBank");
        check("visible: the listed activity is", "dev.agentroid.bankguard".equals(v.bank(BANKS)));
        v.event(GuardLogic.EV_DESTROYED, "dev.agentroid.bankguard", "dev.agentroid.bankguard.TestBank");
        check("visible: destroyed is gone, top cleared", v.bank(BANKS) == null && v.top == null);

        // 18. home Wi-Fi mode: which network is trusted
        Set<String> aps = GuardLogic.parseBssids("AA:BB:CC:DD:EE:01, aa:bb:cc:11:*, nonsense, aa:bb:*, 02:00:00:00:00:00");
        check("bssids: full and 4-octet prefix kept, lower-cased; junk and a 2-octet prefix dropped",
                aps.contains("aa:bb:cc:dd:ee:01") && aps.contains("aa:bb:cc:11:*") && !aps.contains("aa:bb:*") && aps.size() == 3);
        check("trusted: home name, listed access point", GuardLogic.trusted("\"home\"", "aa:bb:cc:dd:ee:01", "home", aps));
        check("trusted: home name, an access point under the prefix", GuardLogic.trusted("\"home\"", "AA:BB:CC:11:22:33", "home", aps));
        check("not trusted: home name, a stranger's access point (a copied name)", !GuardLogic.trusted("\"home\"", "aa:bb:cc:dd:ee:02", "home", aps));
        check("not trusted: another name on a home access point", !GuardLogic.trusted("\"cafe\"", "aa:bb:cc:dd:ee:01", "home", aps));
        check("not trusted: name unreadable (no location access)", !GuardLogic.trusted("<unknown ssid>", "aa:bb:cc:dd:ee:01", "home", aps));
        check("not trusted: access point redacted", !GuardLogic.trusted("\"home\"", "02:00:00:00:00:00", "home", GuardLogic.parseBssids("02:00:00:*")));
        check("not trusted: access point null", !GuardLogic.trusted("\"home\"", null, "home", aps));
        check("not trusted: home mode off (no name)", !GuardLogic.trusted("\"home\"", "aa:bb:cc:dd:ee:01", "", aps));
        check("not trusted: a name with no access points", !GuardLogic.trusted("\"home\"", "aa:bb:cc:dd:ee:01", "home", GuardLogic.parseBssids("")));
        check("not trusted: case matters in the name", !GuardLogic.trusted("\"Home\"", "aa:bb:cc:dd:ee:01", "home", aps));

        // 19. home Wi-Fi mode: what it writes
        st = new State();
        Seen off = new Seen(null, null, 0, false, false, false, 0, true, false);
        Seen on = new Seen(null, null, 0, false, true, true, NET, true, false);
        x = GuardLogic.withHome(Action.NONE, st, off, true);
        check("home: joined the home network, nothing open: wireless on", "home_wifi_on".equals(x.event) && is(x.wifi, 1) && x.usb == null);
        x = GuardLogic.withHome(Action.NONE, st, on, true);
        check("home: already on at home: nothing", x.event == null && !x.writes());
        x = GuardLogic.withHome(Action.NONE, st, on, false);
        check("home: left the home network: wireless off", "home_wifi_off_away".equals(x.event) && is(x.wifi, 0));
        x = GuardLogic.withHome(Action.NONE, st, off, false);
        check("home: away and off: nothing", x.event == null && !x.writes());
        // a bank opened at home: the hold switches both off; home mode agrees for Wireless
        st = new State();
        x = GuardLogic.withHome(GuardLogic.tick(st, bank(1_000, true, true)), st, bank(1_000, true, true), true);
        check("home: bank opened at home: usb=0 and wifi=0", "hold".equals(x.event) && is(x.usb, 0) && is(x.wifi, 0));
        x = GuardLogic.withHome(GuardLogic.tick(st, bank(1_400, false, true)), st, bank(1_400, false, true), true);
        check("home: switched on during the bank: off again", "reassert".equals(x.event) && is(x.wifi, 0));
        x = GuardLogic.withHome(Action.NONE, st, bank(1_500, false, false), true);
        check("home: the network callback during a hold does not switch wireless on", x.event == null && !x.writes());
        x = GuardLogic.withHome(GuardLogic.tick(st, home(1_400 + HG, false, false)), st, home(1_400 + HG, false, false), true);
        check("home: bank closed at home: both back on", "release".equals(x.event) && is(x.usb, 1) && is(x.wifi, 1) && !st.held);
        check("home: keeper goes ahead at home with no bank", !GuardLogic.keeperShouldWaitHome(st, true));
        // a bank held, then the phone leaves home: Wireless stays off at the release, USB comes back
        st = new State();
        GuardLogic.withHome(GuardLogic.tick(st, bank(0, true, true)), st, bank(0, true, true), true);
        check("home: keeper waits while a bank is open at home", GuardLogic.keeperShouldWaitHome(st, true));
        Seen awayHome = new Seen(HOME, null, 10_000, false, false, false, 0, true, true);
        x = GuardLogic.withHome(GuardLogic.tick(st, awayHome), st, awayHome, false);
        check("home: bank closed away from home: usb back, wireless stays off, nothing left-off",
                "release".equals(x.event) && is(x.usb, 1) && x.wifi == null && !st.wifiLeftOff);
        check("home: keeper waits away from home", GuardLogic.keeperShouldWaitHome(st, false));
        // the owner's release button away from home does not switch wireless on
        st = new State();
        GuardLogic.tick(st, bank(0, false, true));
        x = GuardLogic.withHome(GuardLogic.release(st, off, "release_owner"), st, off, false);
        check("home: owner's release away from home: wireless stays off", "release_owner".equals(x.event) && x.wifi == null);

        System.out.println(run + " checks, " + failed + " failed");
        System.exit(failed == 0 ? 0 : 1);
    }
}
