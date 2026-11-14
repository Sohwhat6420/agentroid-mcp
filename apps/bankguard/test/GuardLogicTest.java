import dev.agentroid.bankguard.GuardLogic;
import dev.agentroid.bankguard.GuardLogic.Action;
import dev.agentroid.bankguard.GuardLogic.Seen;
import dev.agentroid.bankguard.GuardLogic.State;

import java.util.Set;

/** Plain-JVM tests for GuardLogic: no Android, no phone. Exit code 0 when all pass. */
public final class GuardLogicTest {
    static int run, failed;
    static final String BANK = "com.uob.mighty.app/com.uob.Main", HOME = "com.miui.home/.Launcher";
    static final Set<String> BANKS = GuardLogic.parseList("com.uob.mighty.app, sg.ndi.sp,dev.agentroid.bankguard/dev.agentroid.bankguard.TestBank");
    static final long NET = 4242;

    static void check(String name, boolean ok) {
        run++;
        if (!ok) { failed++; System.out.println("FAIL " + name); } else System.out.println("ok   " + name);
    }

    static Seen at(String fg, long t, boolean usb, boolean wifi) { return new Seen(fg, t, usb, wifi, true, NET, true); }

    static boolean is(Integer v, int want) { return v != null && v == want; }

    public static void main(String[] a) {
        // 1. a bank in front with both on: hold, both to 0, remember both
        State st = new State();
        Action x = GuardLogic.tick(st, at(BANK, 1_000, true, true), BANKS);
        check("hold writes usb=0 and wifi=0", "hold".equals(x.event) && is(x.usb, 0) && is(x.wifi, 0));
        check("hold remembers both and the network", st.held && st.restoreUsb && st.restoreWifi && st.network == NET);
        check("held package is the package, not the activity", "com.uob.mighty.app".equals(st.heldPkg));

        // 2. something (the keeper) switches wifi back on while held: re-assert
        x = GuardLogic.tick(st, at(BANK, 1_400, false, true), BANKS);
        check("re-assert wifi=0 only", "reassert".equals(x.event) && x.usb == null && is(x.wifi, 0));
        x = GuardLogic.tick(st, at(BANK, 1_800, false, false), BANKS);
        check("nothing to do while held and off", x.event == null && !x.writes());

        // 3. leave to the launcher: nothing before the grace, release after it, same network
        x = GuardLogic.tick(st, at(HOME, 1_800 + GuardLogic.GRACE_MS - 1, false, false), BANKS);
        check("no release inside the grace", x.event == null && st.held);
        x = GuardLogic.tick(st, at(HOME, 1_800 + GuardLogic.GRACE_MS, false, false), BANKS);
        check("release puts both back", "release".equals(x.event) && is(x.usb, 1) && is(x.wifi, 1) && !st.held);
        x = GuardLogic.tick(st, at(HOME, 9_000, true, true), BANKS);
        check("after release nothing more", x.event == null);

        // 4. a prompt over the bank (systemui, biometrics) keeps the hold past the grace; so does an unknown app
        st = new State();
        GuardLogic.tick(st, at(BANK, 0, true, true), BANKS);
        x = GuardLogic.tick(st, at("com.android.systemui/.Shade", 60_000, false, false), BANKS);
        check("pass-through keeps the hold", x.event == null && st.held);
        x = GuardLogic.tick(st, new Seen(null, 120_000, false, false, true, NET, true), BANKS);
        check("unknown foreground keeps the hold", x.event == null && st.held);

        // 5. debugging already off: hold writes nothing and release writes nothing
        st = new State();
        x = GuardLogic.tick(st, at(BANK, 0, false, false), BANKS);
        check("hold with both off writes nothing", "hold".equals(x.event) && !x.writes());
        x = GuardLogic.tick(st, at(HOME, 10_000, false, false), BANKS);
        check("never switches on what it did not switch off", "release".equals(x.event) && !x.writes());

        // 6. only USB was on: only USB comes back
        st = new State();
        GuardLogic.tick(st, at(BANK, 0, true, false), BANKS);
        x = GuardLogic.tick(st, at(HOME, 10_000, false, false), BANKS);
        check("only usb restored", is(x.usb, 1) && x.wifi == null);

        // 7. the Wi-Fi network changed, or is gone: wireless debugging stays off and the owner is told
        st = new State();
        GuardLogic.tick(st, at(BANK, 0, true, true), BANKS);
        x = GuardLogic.tick(st, new Seen(HOME, 10_000, false, false, true, NET + 1, true), BANKS);
        check("other network: usb back, wifi left off", is(x.usb, 1) && x.wifi == null
                && "release_wifi_left_off".equals(x.event) && st.wifiLeftOff);
        st = new State();
        GuardLogic.tick(st, at(BANK, 0, true, true), BANKS);
        x = GuardLogic.tick(st, new Seen(HOME, 10_000, false, false, false, 0, true), BANKS);
        check("off Wi-Fi: wifi left off", x.wifi == null && st.wifiLeftOff);
        st = new State();
        GuardLogic.tick(st, new Seen(BANK, 0, true, true, false, 0, true), BANKS); // held on mobile data
        x = GuardLogic.tick(st, at(HOME, 10_000, false, false), BANKS);
        check("held off Wi-Fi then on Wi-Fi: still left off", x.wifi == null && st.wifiLeftOff);

        // 8. the screen goes off with the bank open: hold for SCREEN_OFF_RELEASE_MS, then release
        st = new State();
        GuardLogic.tick(st, at(BANK, 0, true, true), BANKS);
        x = GuardLogic.tick(st, new Seen(BANK, 1_000, false, false, true, NET, false), BANKS);
        check("screen off: still held", x.event == null && st.held && st.screenOffAt == 1_000);
        x = GuardLogic.tick(st, new Seen(BANK, 1_000 + GuardLogic.SCREEN_OFF_RELEASE_MS - 1, false, false, true, NET, false), BANKS);
        check("screen off: no release before the limit", x.event == null && st.held);
        x = GuardLogic.tick(st, new Seen(BANK, 1_000 + GuardLogic.SCREEN_OFF_RELEASE_MS, false, false, true, NET, false), BANKS);
        check("screen off: release at the limit", "release_screen_off".equals(x.event) && is(x.usb, 1) && is(x.wifi, 1));
        st = new State();
        GuardLogic.tick(st, at(BANK, 0, true, true), BANKS);
        GuardLogic.tick(st, new Seen(BANK, 1_000, false, false, true, NET, false), BANKS);
        x = GuardLogic.tick(st, at(BANK, 2_000, false, false), BANKS);
        check("screen back on in the bank: the off timer resets", x.event == null && st.held && st.screenOffAt == 0);

        // 9. one bank to another stays held
        st = new State();
        GuardLogic.tick(st, at(BANK, 0, true, true), BANKS);
        x = GuardLogic.tick(st, at("sg.ndi.sp/.Main", 500, false, false), BANKS);
        check("bank to bank: still held, package follows", x.event == null && st.held && "sg.ndi.sp".equals(st.heldPkg));

        // 10. list entries: a package matches any activity; an activity entry matches only that activity
        String tb = "dev.agentroid.bankguard/dev.agentroid.bankguard.TestBank";
        check("activity entry matches", GuardLogic.isBank(BANKS, tb));
        check("other activity of that app is not a bank", !GuardLogic.isBank(BANKS, "dev.agentroid.bankguard/dev.agentroid.bankguard.MainActivity"));
        check("package entry matches any activity", GuardLogic.isBank(BANKS, "com.uob.mighty.app/x.Y"));
        check("null is not a bank", !GuardLogic.isBank(BANKS, null));
        Set<String> bad = GuardLogic.parseList("a, ../etc, com.x;rm, com..x, com.ok.app, com.ok.app/..Evil, com.ok.app/a.B$C");
        check("invalid entries dropped", bad.size() == 2 && bad.contains("com.ok.app") && bad.contains("com.ok.app/a.B$C"));

        // 11. release by hand, and release when nothing is held
        st = new State();
        GuardLogic.tick(st, at(BANK, 0, true, true), BANKS);
        x = GuardLogic.release(st, at(BANK, 100, false, false), "release_owner");
        check("owner's release puts both back", "release_owner".equals(x.event) && is(x.usb, 1) && is(x.wifi, 1) && !st.held);
        x = GuardLogic.release(st, at(BANK, 200, false, false), "release_owner");
        check("release with nothing held does nothing", x.event == null && !x.writes());
        st = new State();
        GuardLogic.tick(st, at(BANK, 0, true, true), BANKS);
        x = GuardLogic.release(st, at(HOME, 100, true, true), "release_boot");
        check("already back on: no write", "release_boot".equals(x.event) && !x.writes());

        // 12. the provider's answer goes stale
        check("fresh hold is live", GuardLogic.holdIsLive(true, 1_000, 1_000 + GuardLogic.HOLD_STALE_MS - 1));
        check("stale hold is void", !GuardLogic.holdIsLive(true, 1_000, 1_000 + GuardLogic.HOLD_STALE_MS));
        check("no hold is not live", !GuardLogic.holdIsLive(false, 1_000, 1_000));
        check("a clock that went back is not live", !GuardLogic.holdIsLive(true, 5_000, 1_000));

        System.out.println(run + " checks, " + failed + " failed");
        System.exit(failed == 0 ? 0 : 1);
    }
}
