import dev.agentroid.keeper.KeeperLogic;
import dev.agentroid.keeper.KeeperLogic.Outcome;

/**
 * Plain-JVM unit tests for the keeper's pure logic. No JUnit needed: run with
 *   javac -d out/test src/dev/agentroid/keeper/KeeperLogic.java test/KeeperLogicTest.java
 *   java  -cp out/test KeeperLogicTest
 * Exit code 0 = all pass, 1 = a failure (printed).
 */
public class KeeperLogicTest {
    static int passed = 0, failed = 0;

    static void check(String name, boolean cond) {
        if (cond) { passed++; System.out.println("PASS " + name); }
        else { failed++; System.out.println("FAIL " + name); }
    }
    static void eq(String name, Object a, Object b) {
        check(name + " (" + a + " == " + b + ")", a == null ? b == null : a.equals(b));
    }

    public static void main(String[] args) {
        // ---- loop decision function ----
        eq("evaluateRun off->on = ENABLED", KeeperLogic.evaluateRun(0, 1), Outcome.ENABLED);
        eq("evaluateRun on->on = ALREADY_ON", KeeperLogic.evaluateRun(1, 1), Outcome.ALREADY_ON);
        eq("evaluateRun off->off = UNTRUSTED", KeeperLogic.evaluateRun(0, 0), Outcome.UNTRUSTED_BSSID);
        eq("evaluateRun on->off = UNTRUSTED", KeeperLogic.evaluateRun(1, 0), Outcome.UNTRUSTED_BSSID);
        check("shouldWriteEnable is always true (writes 1 every run)",
                KeeperLogic.shouldWriteEnable(0) && KeeperLogic.shouldWriteEnable(1));
        eq("log line for untrusted matches brief", KeeperLogic.logLineFor(Outcome.UNTRUSTED_BSSID), "adb_wifi: untrusted_bssid");

        // ---- backoff: capped exponential doubling, floor 1000, cap 30000 ----
        eq("backoff floor from 0", KeeperLogic.nextBackoffMs(0), 2000);      // treated as 1000, doubled
        eq("backoff 1000 -> 2000", KeeperLogic.nextBackoffMs(1000), 2000);
        eq("backoff 2000 -> 4000", KeeperLogic.nextBackoffMs(2000), 4000);
        eq("backoff 16000 -> 30000 (cap)", KeeperLogic.nextBackoffMs(16000), 30000);
        eq("backoff 30000 stays 30000", KeeperLogic.nextBackoffMs(30000), 30000);
        eq("resetBackoffMs = 1000", KeeperLogic.resetBackoffMs(), 1000);

        // ---- full-jitter backoff: delay in [0, window], window = min(cap, base*2^attempt) ----
        eq("jitter attempt0 rand0 = 0", KeeperLogic.fullJitterBackoff(0, 0.0, 1000, 30000), 0);
        eq("jitter attempt0 rand~1 ~ base", KeeperLogic.fullJitterBackoff(0, 0.999999, 1000, 30000), 999);
        eq("jitter attempt3 rand0.5 = half of 8000", KeeperLogic.fullJitterBackoff(3, 0.5, 1000, 30000), 4000);
        check("jitter is capped at cap",
                KeeperLogic.fullJitterBackoff(20, 0.999999, 1000, 30000) <= 30000);
        check("jitter never negative", KeeperLogic.fullJitterBackoff(-5, -1.0, 1000, 30000) >= 0);

        // ---- heartbeat payload builder: exact field set and order, no secrets ----
        String hb = KeeperLogic.buildHeartbeat("abc123", 41235, 1, "deadbeef", 87, 1760000000000L);
        eq("heartbeat exact", hb,
                "{\"serial_hint\":\"abc123\",\"port\":41235,\"adb_wifi_enabled\":1,"
                + "\"wifi_ssid_hash\":\"deadbeef\",\"battery\":87,\"ts\":1760000000000}");
        check("heartbeat has no token field", !hb.contains("token"));
        check("heartbeat has no raw ssid field", !hb.contains("\"ssid\":"));
        check("heartbeat has exactly 6 keys", countColons(hb) == 6);

        String hbNulls = KeeperLogic.buildHeartbeat(null, -1, 0, null, -1, 0L);
        eq("heartbeat with nulls/unknowns", hbNulls,
                "{\"serial_hint\":null,\"port\":-1,\"adb_wifi_enabled\":0,"
                + "\"wifi_ssid_hash\":null,\"battery\":-1,\"ts\":0}");
        eq("adb_wifi_enabled coerced to 0/1", KeeperLogic.buildHeartbeat(null, 0, 5, null, 0, 0L),
                "{\"serial_hint\":null,\"port\":0,\"adb_wifi_enabled\":1,\"wifi_ssid_hash\":null,\"battery\":0,\"ts\":0}");

        // ---- ssid hashing: deterministic, salted, 64 hex chars, differs by salt, null->null ----
        String h1 = KeeperLogic.ssidHash("HomeWifi", "salt1");
        String h2 = KeeperLogic.ssidHash("HomeWifi", "salt1");
        String h3 = KeeperLogic.ssidHash("HomeWifi", "salt2");
        eq("ssidHash deterministic", h1, h2);
        check("ssidHash salted (different salt -> different hash)", !h1.equals(h3));
        check("ssidHash is 64 hex chars", h1.matches("[0-9a-f]{64}"));
        check("ssidHash null -> null", KeeperLogic.ssidHash(null, "x") == null);
        check("ssidHash is not the raw ssid", !h1.contains("HomeWifi"));

        // ---- v0.2 pause ----
        check("shouldSkipRun skips only when paused",
                KeeperLogic.shouldSkipRun(true) && !KeeperLogic.shouldSkipRun(false));

        long D = 7_200_000L, M = 43_200_000L;
        eq("nudge: first after 15 s", KeeperLogic.nudgeDelayMs(0), 15_000L);
        eq("nudge: then every 45 s", KeeperLogic.nudgeDelayMs(5), 45_000L);
        eq("nudge: stops after 12", KeeperLogic.nudgeDelayMs(12), -1L);
        eq("nudge: junk count stops", KeeperLogic.nudgeDelayMs(-3), -1L);
        check("pauseMillis 1 -> default", KeeperLogic.pauseMillis("1", null, D, M) == D);
        check("pauseMillis true/yes accepted", KeeperLogic.pauseMillis("true", null, D, M) == D && KeeperLogic.pauseMillis("YES", null, D, M) == D);
        check("pauseMillis 0/false/off -> resume", KeeperLogic.pauseMillis("0", null, D, M) == 0 && KeeperLogic.pauseMillis("off", null, D, M) == 0);
        check("pauseMillis minutes honoured and capped", KeeperLogic.pauseMillis("1", "30", D, M) == 1_800_000L && KeeperLogic.pauseMillis("1", "99999", D, M) == M);
        check("pauseMillis junk -> -1 (ignored, never a silent pause)", KeeperLogic.pauseMillis("maybe", null, D, M) == -1 && KeeperLogic.pauseMillis(null, null, D, M) == -1
                && KeeperLogic.pauseMillis("1", "abc", D, M) == -1 && KeeperLogic.pauseMillis("1", "0", D, M) == -1 && KeeperLogic.pauseMillis("1", "-5", D, M) == -1);

        // ---- json escaping ----
        eq("jsonStr escapes quotes/backslash", KeeperLogic.jsonStr("a\"b\\c"), "\"a\\\"b\\\\c\"");
        eq("jsonStr null", KeeperLogic.jsonStr(null), "null");

        System.out.println("---- " + passed + " passed, " + failed + " failed ----");
        System.exit(failed == 0 ? 0 : 1);
    }

    static int countColons(String json) {
        int n = 0;
        boolean inStr = false;
        for (int i = 0; i < json.length(); i++) {
            char c = json.charAt(i);
            if (c == '"' && (i == 0 || json.charAt(i - 1) != '\\')) inStr = !inStr;
            else if (c == ':' && !inStr) n++;
        }
        return n;
    }
}
