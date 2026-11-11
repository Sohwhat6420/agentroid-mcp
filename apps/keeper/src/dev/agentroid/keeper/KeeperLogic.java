package dev.agentroid.keeper;

import java.nio.charset.StandardCharsets;
import java.security.MessageDigest;

/**
 * Pure keeper logic: no Android imports, so it runs on a plain JVM and is unit tested
 * (see test/KeeperLogicTest.java). All methods are static and side-effect free.
 *
 * Covers: the loop's decision function (evaluateRun), the heartbeat payload builder
 * (buildHeartbeat), the reconnect backoff (fullJitterBackoff / nextBackoffMs) and the
 * SSID hashing that keeps the payload secret-free (ssidHash).
 */
public final class KeeperLogic {
    private KeeperLogic() {}

    // ---------------------------------------------------------------- loop decision

    /** Outcome of one keeper run, derived from adb_wifi_enabled before the write and the readback within 5 s. */
    public enum Outcome {
        /** Was already 1 before we wrote; nothing was wrong. */
        ALREADY_ON,
        /** We wrote 1 and it stuck (read back 1). */
        ENABLED,
        /** We wrote 1 but it reverted to 0 within 5 s: AdbDebuggingManager rejected the BSSID. */
        UNTRUSTED_BSSID
    }

    /**
     * The loop's decision function.
     * @param valueBefore   adb_wifi_enabled read at the start of the run (0 or 1; any other treated as 0)
     * @param valueAfter5s  adb_wifi_enabled read back within 5 s of writing 1 (0 or 1)
     */
    public static Outcome evaluateRun(int valueBefore, int valueAfter5s) {
        if (valueAfter5s == 1) {
            return valueBefore == 1 ? Outcome.ALREADY_ON : Outcome.ENABLED;
        }
        return Outcome.UNTRUSTED_BSSID;
    }

    /** True if the run should always issue the write. The keeper writes 1 on every run regardless of state. */
    public static boolean shouldWriteEnable(int valueBefore) {
        return true;
    }

    /** v0.2: parse the pause extras. Returns ms to pause, 0 to resume, -1 if not understood. Max is a hard cap. */
    public static long pauseMillis(String pause, String minutes, long defaultMs, long maxMs) {
        if (pause == null) return -1;
        String p = pause.trim().toLowerCase(java.util.Locale.ROOT);
        if (p.equals("0") || p.equals("false") || p.equals("no") || p.equals("off")) return 0;
        if (!(p.equals("1") || p.equals("true") || p.equals("yes") || p.equals("on"))) return -1;
        long ms = defaultMs;
        if (minutes != null) {
            try { ms = Long.parseLong(minutes.trim()) * 60_000L; } catch (NumberFormatException e) { return -1; }
            if (ms <= 0) return -1;
        }
        return Math.min(ms, maxMs);
    }

    /** v0.2: a paused keeper (`--es pause 1`) skips its run and its heartbeat but stays alive and keeps the alarm chain. */
    /** Screen-wake nudges while the default network is gone (HyperOS defers a Wi-Fi rejoin while the screen is off):
     *  first after 15 s, then every 45 s, at most 12 times. Returns the delay before nudge number {@code sent}, or -1 to stop. */
    public static long nudgeDelayMs(int sent) {
        if (sent < 0 || sent >= 12) return -1;
        return sent == 0 ? 15_000L : 45_000L;
    }

    public static boolean shouldSkipRun(boolean paused) {
        return paused;
    }

    /** The log line emitted when a run reverts. Matches the brief's required string. */
    public static String logLineFor(Outcome o) {
        switch (o) {
            case UNTRUSTED_BSSID: return "adb_wifi: untrusted_bssid";
            case ENABLED:         return "adb_wifi: enabled";
            case ALREADY_ON:      return "adb_wifi: already_on";
            default:              return "adb_wifi: unknown";
        }
    }

    // ---------------------------------------------------------------- backoff

    /** Capped exponential doubling: floor 1000 ms, cap 30000 ms. Used as the base before jitter. */
    public static int nextBackoffMs(int currentMs) {
        int base = currentMs < 1000 ? 1000 : currentMs;
        long doubled = (long) base * 2L;
        if (doubled > 30_000L) return 30_000;
        return (int) doubled;
    }

    /**
     * Full-jitter backoff (research design step 5: "full-jitter, capped at 30 s, reset on any network-change").
     * delay = random in [0, min(cap, base*2^attempt)].
     * @param attempt   0-based retry count
     * @param rand      a value in [0,1) (e.g. Math.random()); made a param so the test is deterministic
     */
    public static int fullJitterBackoff(int attempt, double rand, int baseMs, int capMs) {
        if (attempt < 0) attempt = 0;
        long window = baseMs;
        for (int i = 0; i < attempt && window < capMs; i++) window *= 2L;
        if (window > capMs) window = capMs;
        if (rand < 0) rand = 0;
        if (rand >= 1) rand = 0.999999;
        return (int) Math.floor(rand * window);
    }

    /** On a network-change callback the backoff resets to its floor. */
    public static int resetBackoffMs() {
        return 1000;
    }

    // ---------------------------------------------------------------- heartbeat payload

    /**
     * Build the heartbeat JSON. No secrets: the SSID is hashed, no token, no serial (only a hint).
     * Field order is fixed for test stability.
     * @param serialHint  a non-identifying hint (e.g. ANDROID_ID); may be null
     * @param port        discovered adb-tls port, or -1 if unknown
     * @param adbWifiEnabled 0/1
     * @param wifiSsidHash hex sha256 of ssid+salt, or null
     * @param batteryPct  0-100, or -1 if unknown
     * @param ts          epoch millis
     */
    public static String buildHeartbeat(String serialHint, int port, int adbWifiEnabled,
                                        String wifiSsidHash, int batteryPct, long ts) {
        StringBuilder b = new StringBuilder(160);
        b.append('{');
        b.append("\"serial_hint\":").append(jsonStr(serialHint)).append(',');
        b.append("\"port\":").append(port).append(',');
        b.append("\"adb_wifi_enabled\":").append(adbWifiEnabled != 0 ? 1 : 0).append(',');
        b.append("\"wifi_ssid_hash\":").append(jsonStr(wifiSsidHash)).append(',');
        b.append("\"battery\":").append(batteryPct).append(',');
        b.append("\"ts\":").append(ts);
        b.append('}');
        return b.toString();
    }

    /** sha256 hex of (ssid + "|" + salt). The salt keeps the hash from being a rainbow-table lookup of common SSIDs. */
    public static String ssidHash(String ssid, String salt) {
        if (ssid == null) return null;
        try {
            MessageDigest md = MessageDigest.getInstance("SHA-256");
            byte[] d = md.digest((ssid + "|" + (salt == null ? "" : salt)).getBytes(StandardCharsets.UTF_8));
            StringBuilder sb = new StringBuilder(64);
            for (byte x : d) sb.append(Character.forDigit((x >> 4) & 0xf, 16)).append(Character.forDigit(x & 0xf, 16));
            return sb.toString();
        } catch (Exception e) {
            return null;
        }
    }

    /** Minimal JSON string escaper (null -> literal null). */
    public static String jsonStr(String s) {
        if (s == null) return "null";
        StringBuilder b = new StringBuilder(s.length() + 2);
        b.append('"');
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            switch (c) {
                case '"':  b.append("\\\""); break;
                case '\\': b.append("\\\\"); break;
                case '\n': b.append("\\n");  break;
                case '\r': b.append("\\r");  break;
                case '\t': b.append("\\t");  break;
                default:
                    if (c < 0x20) b.append(String.format("\\u%04x", (int) c));
                    else b.append(c);
            }
        }
        b.append('"');
        return b.toString();
    }
}
