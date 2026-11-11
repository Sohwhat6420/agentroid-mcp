package dev.agentroid.helper.keeper.logic;

import java.util.Collection;
import java.util.Collections;
import java.util.LinkedHashSet;
import java.util.Locale;
import java.util.Set;
import java.util.regex.Pattern;

/** BSSID forms: lower-case colon MAC. Android's redacted placeholder 02:00:00:00:00:00 means "cannot read". */
public final class Bssids {
    private Bssids() { }

    public static final String REDACTED = "02:00:00:00:00:00";
    public static final int MAX_TRUSTED = 64;
    private static final Pattern MAC = Pattern.compile("[0-9a-f]{2}(:[0-9a-f]{2}){5}");

    public static final class Invalid extends Exception {
        public Invalid(String m) { super(m); }
    }

    /** The readable BSSID in canonical form, or null when it is missing, redacted or malformed. */
    public static String readable(String raw) {
        if (raw == null) return null;
        String b = raw.trim().toLowerCase(Locale.ROOT);
        if (!MAC.matcher(b).matches() || b.equals(REDACTED) || b.equals("00:00:00:00:00:00") || b.equals("ff:ff:ff:ff:ff:ff")) return null;
        return b;
    }

    /** The hub's trusted list: every entry must be a real BSSID; at most 64 (one per mesh AP). */
    public static Set<String> parseTrusted(Collection<String> in) throws Invalid {
        if (in == null) throw new Invalid("bssids is missing");
        Set<String> out = new LinkedHashSet<String>();
        for (String s : in) {
            String b = readable(s);
            if (b == null) throw new Invalid("not a usable BSSID: " + (s == null ? "null" : s.length() > 40 ? s.substring(0, 40) : s));
            out.add(b);
        }
        if (out.size() > MAX_TRUSTED) throw new Invalid("at most " + MAX_TRUSTED + " trusted BSSIDs");
        return Collections.unmodifiableSet(out);
    }
}
