package dev.agentroid.helper.core;

import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * Where the phone dials, and what the notification calls the laptop. ADR 0001: plain ws:// is acceptable only inside
 * the tailnet (WireGuard encrypts), so the host must be a tailnet address (100.64.0.0/10), a *.ts.net name, or loopback
 * (for `adb reverse` during bring-up). Anything else is refused here, so a typo or a hostile config cannot send
 * cleartext traffic across the LAN or the internet.
 */
public final class HubSpec {
    public static final int DEFAULT_PORT = 8765;
    public static final int MAX_LABEL_CODEPOINTS = 40;
    public static final String PATH = "/helper";

    private static final Pattern HOST = Pattern.compile("[A-Za-z0-9.-]{1,253}");
    private static final Pattern IPV4 = Pattern.compile("(\\d{1,3})\\.(\\d{1,3})\\.(\\d{1,3})\\.(\\d{1,3})");

    public static final class Invalid extends Exception {
        public Invalid(String m) { super(m); }
    }

    public final String host;
    public final int port;
    /** Safe to show in the notification: no control or bidi/format characters, at most 40 code points. */
    public final String laptopLabel;

    private HubSpec(String host, int port, String label) {
        this.host = host;
        this.port = port;
        this.laptopLabel = label;
    }

    public String url() { return "ws://" + host + ":" + port + PATH; }

    /** @param hub "host" or "host:port"; @param laptop display name, may be null */
    public static HubSpec parse(String hub, String laptop) throws Invalid {
        if (hub == null) throw new Invalid("hub address is missing");
        String h = hub.trim();
        int port = DEFAULT_PORT;
        int colon = h.lastIndexOf(':');
        if (colon >= 0) {
            String ps = h.substring(colon + 1);
            if (!ps.matches("\\d{1,5}")) throw new Invalid("port must be digits");
            port = Integer.parseInt(ps);
            if (port < 1 || port > 65535) throw new Invalid("port must be 1..65535");
            h = h.substring(0, colon);
        }
        if (!HOST.matcher(h).matches()) throw new Invalid("host has characters that are not allowed");
        h = h.toLowerCase(java.util.Locale.ROOT);
        if (!allowedHost(h)) {
            throw new Invalid("host is not on the tailnet (100.64.0.0/10, *.ts.net) or loopback; plain ws:// is refused elsewhere");
        }
        return new HubSpec(h, port, label(laptop, h));
    }

    static boolean allowedHost(String h) {
        if (h.equals("localhost")) return true;
        Matcher m = IPV4.matcher(h);
        if (m.matches()) {
            int[] o = new int[4];
            for (int i = 0; i < 4; i++) {
                String part = m.group(i + 1);
                if (part.length() > 1 && part.startsWith("0")) return false; // no octal-looking forms
                o[i] = Integer.parseInt(part);
                if (o[i] > 255) return false;
            }
            if (o[0] == 127) return true;
            return o[0] == 100 && o[1] >= 64 && o[1] <= 127;
        }
        if (h.endsWith(".ts.net") && h.length() > ".ts.net".length() && !h.startsWith(".") && !h.contains("..")) return true;
        return false;
    }

    static String label(String laptop, String fallback) {
        if (laptop == null) return fallback;
        StringBuilder b = new StringBuilder();
        int count = 0;
        for (int i = 0; i < laptop.length() && count < MAX_LABEL_CODEPOINTS; ) {
            int cp = laptop.codePointAt(i);
            i += Character.charCount(cp);
            int type = Character.getType(cp);
            if (Character.isISOControl(cp) || type == Character.FORMAT || type == Character.LINE_SEPARATOR
                    || type == Character.PARAGRAPH_SEPARATOR || type == Character.SURROGATE
                    || type == Character.UNASSIGNED || type == Character.PRIVATE_USE) {
                continue;
            }
            b.appendCodePoint(cp);
            count++;
        }
        String s = b.toString().trim();
        return s.isEmpty() ? fallback : s;
    }
}
