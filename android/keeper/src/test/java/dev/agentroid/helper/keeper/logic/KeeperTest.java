package dev.agentroid.helper.keeper.logic;

import java.io.File;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashMap;
import java.util.HashSet;
import java.util.LinkedHashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;

import javax.xml.XMLConstants;
import javax.xml.parsers.DocumentBuilderFactory;

import org.w3c.dom.Document;
import org.w3c.dom.Element;
import org.w3c.dom.Node;
import org.w3c.dom.NodeList;

/** Keeper: untrusted BSSID reports and does not re-assert; trusted re-asserts; the :keeper permission list is exactly five. */
public class KeeperTest {
    static final String COMP = "dev.agentroid.helper/dev.agentroid.helper.a11y.HelperAccessibilityService";
    static final String HOME = "aa:bb:cc:dd:ee:01", MESH2 = "aa:bb:cc:dd:ee:02", CAFE = "11:22:33:44:55:66";

    static void ok(boolean c, String m) { if (!c) throw new AssertionError(m); }

    static void eq(Object e, Object a, String m) {
        if (e == null ? a != null : !e.equals(a)) throw new AssertionError(m + ": expected <" + e + "> but was <" + a + ">");
    }

    static final class FakeSettings implements Keeper.Settings {
        final Map<String, String> global = new HashMap<String, String>(), secure = new HashMap<String, String>();
        final List<String> writes = new ArrayList<String>();
        boolean denied;
        public String getGlobal(String k) { return global.get(k); }
        public void putGlobal(String k, String v) {
            if (denied) throw new SecurityException("WRITE_SECURE_SETTINGS not granted");
            writes.add("global " + k + "=" + v);
            global.put(k, v);
        }
        public String getSecure(String k) { return secure.get(k); }
        public void putSecure(String k, String v) {
            if (denied) throw new SecurityException("WRITE_SECURE_SETTINGS not granted");
            writes.add("secure " + k + "=" + v);
            secure.put(k, v);
        }
        int adbWrites() {
            int n = 0;
            for (String w : writes) if (w.startsWith("global adb_wifi_enabled")) n++;
            return n;
        }
    }

    static final class FakeWifi implements Keeper.Wifi {
        boolean on = true;
        String bssid;
        public boolean onWifi() { return on; }
        public String bssid() { return bssid; }
    }

    static final class FakeTrust implements Keeper.TrustList {
        Set<String> s = new HashSet<String>();
        public Set<String> get() { return s; }
        public void set(Set<String> b) { s = new HashSet<String>(b); }
    }

    static final class Rig implements Keeper.Reporter {
        final TickClock clock = new TickClock();
        final FakeSettings settings = new FakeSettings();
        final FakeWifi wifi = new FakeWifi();
        final FakeTrust trust = new FakeTrust();
        final List<String> reports = new ArrayList<String>();
        final Keeper k = new Keeper(clock, settings, wifi, trust, this, COMP);
        Rig() {
            settings.global.put(Keeper.ADB_WIFI, "0");
            settings.secure.put(Keeper.A11Y_LIST, "com.other/.OtherService");
            settings.secure.put(Keeper.A11Y_ON, "1");
            trust.s.add(HOME);
            trust.s.add(MESH2);
        }
        public void untrustedBssid(String bssid, String reason) { reports.add(reason + " " + bssid); }
    }

    public void testUntrustedBssidReportsAndDoesNotReassert() {
        Rig r = new Rig();
        r.wifi.bssid = CAFE;
        r.k.start();
        r.clock.runFor(24L * 3600 * 1000); // a day on the cafe's Wi-Fi: 96 ticks
        r.k.onSettingChanged();
        r.k.onNetworkChanged();
        eq(0, r.settings.adbWrites(), "adb_wifi_enabled writes on an untrusted BSSID");
        eq("0", r.settings.global.get(Keeper.ADB_WIFI), "wireless debugging left off");
        eq(Collections.singletonList("not_in_trusted_list " + CAFE), r.reports, "reported once (not every tick)");
        eq("untrusted_bssid", r.k.last().state, "state");
        eq(Boolean.FALSE, r.k.beacon().get("bssid_trusted"), "beacon says untrusted");
        eq("untrusted_bssid", r.k.beacon().get("keeper"), "beacon keeper state");
        ok(r.k.passes() >= 97, "it kept checking: " + r.k.passes());
    }

    public void testTrustedBssidReasserts() {
        Rig r = new Rig();
        r.wifi.bssid = "AA:BB:CC:DD:EE:02"; // upper case from some APIs; the second mesh AP
        r.k.start();
        eq("1", r.settings.global.get(Keeper.ADB_WIFI), "re-asserted on boot");
        eq(1, r.settings.adbWrites(), "one write");
        ok(r.reports.isEmpty(), "nothing reported");
        eq("ok", r.k.last().state, "state");
        // something turns wireless debugging off; the ContentObserver path puts it back at once
        r.settings.global.put(Keeper.ADB_WIFI, "0");
        r.k.onSettingChanged();
        eq("1", r.settings.global.get(Keeper.ADB_WIFI), "re-asserted on change");
        // off again without a change callback (missed): the 15-minute tick catches it
        r.settings.global.put(Keeper.ADB_WIFI, "0");
        r.clock.runFor(Keeper.TICK_MS);
        eq("1", r.settings.global.get(Keeper.ADB_WIFI), "re-asserted by the 15-minute tick");
        eq(3, r.settings.adbWrites(), "three writes, one per time it was off");
        r.clock.runFor(4 * Keeper.TICK_MS);
        eq(3, r.settings.adbWrites(), "no write when it is already on (our own write cannot loop)");
    }

    public void testTicksEvery15Minutes() {
        Rig r = new Rig();
        r.wifi.bssid = HOME;
        r.k.start();
        r.clock.runFor(60L * 60 * 1000);
        eq(Arrays.asList("boot", "tick", "tick", "tick", "tick"), r.k.triggers(), "boot plus four ticks in an hour");
        r.k.stop();
        r.clock.runFor(60L * 60 * 1000);
        eq(5, r.k.passes(), "stopped");
    }

    public void testUnreadableBssidAndEmptyListAreUntrusted() {
        Rig r = new Rig();
        r.wifi.bssid = "02:00:00:00:00:00"; // what Android returns without location access
        r.k.start();
        r.wifi.bssid = null;
        r.k.onNetworkChanged();
        r.wifi.bssid = HOME;
        r.trust.s.clear();
        r.k.onNetworkChanged();
        eq(0, r.settings.adbWrites(), "never re-asserted");
        eq(Arrays.asList("bssid_unreadable null", "no_trusted_list " + HOME), r.reports, "reasons");
    }

    public void testRoamingToAnotherUntrustedApReportsAgain() {
        Rig r = new Rig();
        r.wifi.bssid = CAFE;
        r.k.start();
        r.wifi.bssid = "11:22:33:44:55:77";
        r.k.onNetworkChanged();
        r.wifi.bssid = HOME;
        r.k.onNetworkChanged();
        r.wifi.bssid = CAFE;
        r.k.onNetworkChanged();
        eq(3, r.reports.size(), "each untrusted arrival is reported: " + r.reports);
        eq(1, r.settings.adbWrites(), "re-asserted only at home");
    }

    public void testOffWifiNothingIsWrittenForAdb() {
        Rig r = new Rig();
        r.wifi.on = false;
        r.k.start();
        eq(0, r.settings.adbWrites(), "no adb write off Wi-Fi");
        eq("no_wifi", r.k.last().state, "state");
        ok(r.reports.isEmpty(), "nothing to report off Wi-Fi");
    }

    public void testNoPermissionIsReported() {
        Rig r = new Rig();
        r.wifi.bssid = HOME;
        r.settings.denied = true;
        r.k.start();
        eq("no_permission", r.k.last().state, "state when WRITE_SECURE_SETTINGS is not granted");
    }

    public void testAccessibilityListIsMergedNotReplaced() {
        Rig r = new Rig();
        r.wifi.bssid = HOME;
        r.k.start();
        eq("com.other/.OtherService:" + COMP, r.settings.secure.get(Keeper.A11Y_LIST), "ours appended, the other kept");
        int writes = r.settings.writes.size();
        r.k.onSettingChanged();
        eq(writes, r.settings.writes.size(), "idempotent");
        r.settings.secure.put(Keeper.A11Y_LIST, "dev.agentroid.helper/.a11y.HelperAccessibilityService");
        r.k.onSettingChanged();
        eq("dev.agentroid.helper/.a11y.HelperAccessibilityService", r.settings.secure.get(Keeper.A11Y_LIST), "the short form counts as present");
        r.settings.secure.put(Keeper.A11Y_LIST, "");
        r.settings.secure.put(Keeper.A11Y_ON, "0");
        r.k.onSettingChanged();
        eq(COMP, r.settings.secure.get(Keeper.A11Y_LIST), "re-added when removed");
        eq("1", r.settings.secure.get(Keeper.A11Y_ON), "accessibility switched back on");
    }

    public void testAccessibilityGatingFollowsThePolicyConstant() {
        Rig r = new Rig();
        r.wifi.bssid = CAFE;
        r.settings.secure.put(Keeper.A11Y_LIST, "");
        r.k.start();
        boolean readded = COMP.equals(r.settings.secure.get(Keeper.A11Y_LIST));
        eq(!Keeper.A11Y_FOLLOWS_BSSID, readded, "accessibility on an untrusted BSSID follows A11Y_FOLLOWS_BSSID");
        eq(0, r.settings.adbWrites(), "but wireless debugging is never re-asserted there");
    }

    public void testHubSetsTheTrustedList() throws Exception {
        Rig r = new Rig();
        r.wifi.bssid = CAFE;
        r.k.start();
        eq(0, r.settings.adbWrites(), "untrusted first");
        r.k.setTrusted(Arrays.asList(HOME, "11:22:33:44:55:66")); // one tap for the new AP
        eq("1", r.settings.global.get(Keeper.ADB_WIFI), "trusted at once");
        for (List<String> bad : Arrays.asList(Arrays.asList("02:00:00:00:00:00"), Arrays.asList("not a mac"), Arrays.asList(HOME, null))) {
            try {
                r.k.setTrusted(bad);
                throw new AssertionError("accepted " + bad);
            } catch (Bssids.Invalid expected) {
                // refused
            }
        }
        List<String> many = new ArrayList<String>();
        for (int i = 0; i < 65; i++) many.add(String.format("aa:bb:cc:dd:%02x:%02x", i / 256, i % 256));
        try {
            r.k.setTrusted(many);
            throw new AssertionError("accepted 65");
        } catch (Bssids.Invalid expected) {
            // refused
        }
    }

    public void testAdbPortIsOursOnly() {
        List<AdbServices.Found> f = Arrays.asList(
                new AdbServices.Found("adb-OTHERPHONE-abc", "192.0.2.50", 41111),
                new AdbServices.Found("adb-POCO1-xyz", "192.0.2.23", 37555),
                new AdbServices.Found("printer", "192.0.2.23", 631));
        eq(Integer.valueOf(37555), AdbServices.pick(f, Arrays.asList("192.0.2.23", "100.70.1.2"), null), "by own address");
        eq(Integer.valueOf(37555), AdbServices.pick(f, Collections.<String>emptyList(), "POCO1"), "by serial in the name");
        eq(null, AdbServices.pick(f, Arrays.asList("198.51.100.9"), "NOTHERE"), "another phone's port is never reported");
        Rig r = new Rig();
        r.k.setAdbPort(37555);
        eq(Long.valueOf(37555), r.k.beacon().get("adb_port"), "beacon port");
        eq("nsd", r.k.beacon().get("adb_port_source"), "beacon source");
        eq("_adb-tls-connect._tcp", AdbServices.SERVICE_TYPE, "service type");
    }

    /** The :keeper module's own manifest asks for exactly these five and nothing else. */
    public void testKeeperManifestAsksForExactlyTheFivePermissions() throws Exception {
        File f = new File(System.getProperty("helper.root", "android"), "keeper/src/main/AndroidManifest.xml");
        DocumentBuilderFactory fac = DocumentBuilderFactory.newInstance();
        fac.setNamespaceAware(true);
        fac.setFeature(XMLConstants.FEATURE_SECURE_PROCESSING, true);
        Document d = fac.newDocumentBuilder().parse(f);
        List<String> got = new ArrayList<String>();
        NodeList all = d.getElementsByTagName("*");
        for (int i = 0; i < all.getLength(); i++) {
            Element e = (Element) all.item(i);
            // uses-permission, uses-permission-sdk-23 and any future variant all count
            if (e.getTagName().startsWith("uses-permission")) got.add(e.getAttributeNS("http://schemas.android.com/apk/res/android", "name"));
            ok(!e.getTagName().equals("permission"), "keeper must not define permissions");
        }
        Set<String> want = new LinkedHashSet<String>();
        for (String p : new String[] {"RECEIVE_BOOT_COMPLETED", "FOREGROUND_SERVICE", "ACCESS_NETWORK_STATE", "WRITE_SECURE_SETTINGS", "INTERNET"}) {
            want.add("android.permission." + p);
        }
        eq(5, got.size(), "uses-permission entries in :keeper (no sixth, no duplicates): " + got);
        eq(want, new LinkedHashSet<String>(got), "the five");
        // when Gradle has built the module, its processed library manifest (what :keeper contributes to the merge) too
        File built = new File(System.getProperty("helper.root", "android"),
                "keeper/build/intermediates/merged_manifest/debug/processDebugManifest/AndroidManifest.xml");
        if (built.isFile()) {
            Document b = fac.newDocumentBuilder().parse(built);
            List<String> bg = new ArrayList<String>();
            NodeList ball = b.getElementsByTagName("*");
            for (int i = 0; i < ball.getLength(); i++) {
                Element e = (Element) ball.item(i);
                if (e.getTagName().startsWith("uses-permission")) bg.add(e.getAttributeNS("http://schemas.android.com/apk/res/android", "name"));
            }
            eq(want, new LinkedHashSet<String>(bg), "Gradle-built :keeper manifest permissions");
            eq(5, bg.size(), "Gradle-built :keeper manifest has five entries: " + bg);
            System.out.println("  keeper: source and Gradle-built manifests both ask for exactly the five");
        }
        // and its one exported receiver is gated by DUMP with a justification comment
        Element app = (Element) d.getElementsByTagName("application").item(0);
        for (Node n = app.getFirstChild(); n != null; n = n.getNextSibling()) {
            if (!(n instanceof Element)) continue;
            Element c = (Element) n;
            String ex = c.getAttributeNS("http://schemas.android.com/apk/res/android", "exported");
            ok(!ex.isEmpty(), c.getAttribute("android:name") + " has no explicit exported");
            if ("true".equals(ex)) {
                eq("android.permission.DUMP", c.getAttributeNS("http://schemas.android.com/apk/res/android", "permission"), "exported keeper receiver permission");
                Node p = c.getPreviousSibling();
                while (p != null && p.getNodeType() == Node.TEXT_NODE) p = p.getPreviousSibling();
                ok(p != null && p.getNodeType() == Node.COMMENT_NODE && p.getNodeValue().trim().startsWith("EXPORTED:"), "EXPORTED: comment");
            }
        }
    }
}
