package dev.agentroid.helper.link.net;

import java.io.UnsupportedEncodingException;
import java.net.URI;
import java.net.URISyntaxException;
import java.net.URLDecoder;
import java.util.HashMap;
import java.util.Map;
import java.util.regex.Pattern;

/**
 * The pairing QR for the no-ADB route (ship/NO-ADB-PLAN.md, stage 4). The laptop shows
 * <pre>
 *   agentroid://pair?h=192.168.1.20:8765&amp;fp=&lt;64 hex: SHA-256 of the hub's TLS certificate&gt;
 *       &amp;k=&lt;22 base64url chars: the one-time 128-bit code&gt;&amp;e=&lt;expiry, Unix seconds&gt;[&amp;n=&lt;laptop name&gt;]
 * </pre>
 * and the phone's own camera app opens it in Agentroid, so the app needs no camera permission.
 *
 * Everything in a QR is attacker-controllable (a printed sticker, a QR in a web page), so this parser is strict:
 * <ul>
 *   <li>exactly the scheme {@code agentroid} and host {@code pair}; no user info, path, fragment or port on the URI;</li>
 *   <li>the hub is an IPv4 literal in a private range (10/8, 172.16/12, 192.168/16) or the tailnet (100.64/10), with a
 *       port; no host names, so a QR cannot point the phone at a name that later resolves elsewhere;</li>
 *   <li>{@code fp} is exactly 32 bytes of lowercase hex; {@code k} is exactly 16 bytes of unpadded base64url;</li>
 *   <li>{@code e} has not passed and is at most {@link #MAX_AHEAD_S} ahead (the code lives 5 minutes);</li>
 *   <li>unknown or repeated parameters are refused, and so is anything over {@link #MAX_LEN} characters.</li>
 * </ul>
 * Parsing proves nothing about the laptop: the TLS pin ({@code fp}), the one-time code and the laptop's own approval
 * prompt do that. A parsed QR is only a request the user still confirms on the phone ("Pair with 'Alex-PC'?").
 */
public final class PairUri {
    public static final int MAX_LEN = 512;
    /** Five-minute code plus clock skew between laptop and phone. */
    public static final long MAX_AHEAD_S = 600;

    private static final Pattern IPV4 = Pattern.compile("(\\d{1,3})\\.(\\d{1,3})\\.(\\d{1,3})\\.(\\d{1,3})");
    private static final Pattern PORT = Pattern.compile("[1-9]\\d{0,4}");
    private static final Pattern HEX64 = Pattern.compile("[0-9a-f]{64}");
    private static final Pattern B64URL_16 = Pattern.compile("[A-Za-z0-9_-]{21}[AQgw]");
    private static final Pattern EPOCH = Pattern.compile("[1-9]\\d{0,10}");
    private static final Pattern NAME = Pattern.compile("[A-Za-z0-9 ._'-]{1,64}");

    public final String host;
    public final int port;
    public final byte[] certSha256;
    public final byte[] code;
    public final long expiresAt;
    /** The laptop's name for the confirm screen, or null. Display only: never trusted. */
    public final String laptop;

    private PairUri(String host, int port, byte[] certSha256, byte[] code, long expiresAt, String laptop) {
        this.host = host;
        this.port = port;
        this.certSha256 = certSha256;
        this.code = code;
        this.expiresAt = expiresAt;
        this.laptop = laptop;
    }

    public static final class Invalid extends Exception {
        public Invalid(String m) { super(m); }
    }

    public static PairUri parse(String s, long nowEpochS) throws Invalid {
        if (s == null || s.isEmpty()) throw new Invalid("empty");
        if (s.length() > MAX_LEN) throw new Invalid("longer than " + MAX_LEN + " characters");
        URI u;
        try {
            u = new URI(s);
        } catch (URISyntaxException e) {
            throw new Invalid("not a URI");
        }
        if (!"agentroid".equals(u.getScheme())) throw new Invalid("scheme must be agentroid");
        if (!"pair".equals(u.getRawAuthority())) throw new Invalid("must be agentroid://pair");
        String path = u.getRawPath();
        if (path != null && !path.isEmpty()) throw new Invalid("no path allowed");
        if (u.getRawFragment() != null) throw new Invalid("no fragment allowed");
        String q = u.getRawQuery();
        if (q == null || q.isEmpty()) throw new Invalid("no parameters");

        Map<String, String> p = new HashMap<String, String>();
        for (String part : q.split("&", -1)) {
            int eq = part.indexOf('=');
            if (eq <= 0) throw new Invalid("malformed parameter");
            String k = part.substring(0, eq);
            String v = part.substring(eq + 1);
            if (!k.equals("h") && !k.equals("fp") && !k.equals("k") && !k.equals("e") && !k.equals("n")) {
                throw new Invalid("unknown parameter " + safe(k));
            }
            if (p.put(k, v) != null) throw new Invalid("repeated parameter " + k);
        }
        for (String k : new String[] {"h", "fp", "k", "e"}) {
            if (!p.containsKey(k) || p.get(k).isEmpty()) throw new Invalid("missing " + k);
        }

        String h = p.get("h");
        int colon = h.lastIndexOf(':');
        if (colon <= 0) throw new Invalid("h must be <IPv4>:<port>");
        String host = h.substring(0, colon);
        String portS = h.substring(colon + 1);
        checkHost(host);
        if (!PORT.matcher(portS).matches()) throw new Invalid("bad port");
        int port = Integer.parseInt(portS);
        if (port > 65535) throw new Invalid("bad port");

        String fp = p.get("fp");
        if (!HEX64.matcher(fp).matches()) throw new Invalid("fp must be 64 lowercase hex characters");
        byte[] pin = new byte[32];
        for (int i = 0; i < 32; i++) pin[i] = (byte) Integer.parseInt(fp.substring(2 * i, 2 * i + 2), 16);

        String k = p.get("k");
        if (!B64URL_16.matcher(k).matches()) throw new Invalid("k must be 16 bytes of unpadded base64url");
        byte[] code = b64url16(k);

        String e = p.get("e");
        if (!EPOCH.matcher(e).matches()) throw new Invalid("e must be Unix seconds");
        long exp = Long.parseLong(e);
        if (exp <= nowEpochS) throw new Invalid("the pairing code has expired; show a new QR on the laptop");
        if (exp > nowEpochS + MAX_AHEAD_S) throw new Invalid("expiry is too far ahead (check the phone's clock)");

        String laptop = null;
        if (p.containsKey("n")) {
            try {
                laptop = URLDecoder.decode(p.get("n"), "UTF-8");
            } catch (UnsupportedEncodingException | IllegalArgumentException ex) {
                throw new Invalid("bad n");
            }
            if (!NAME.matcher(laptop).matches()) throw new Invalid("n must be 1..64 of letters, digits, space . _ ' -");
        }
        return new PairUri(host, port, pin, code, exp, laptop);
    }

    /** Private IPv4 (RFC 1918) or the tailnet's 100.64.0.0/10. Never loopback, link-local, 0.0.0.0 or public. */
    static void checkHost(String host) throws Invalid {
        java.util.regex.Matcher m = IPV4.matcher(host);
        if (!m.matches()) throw new Invalid("h must be an IPv4 address, not a name");
        int[] o = new int[4];
        for (int i = 0; i < 4; i++) {
            String g = m.group(i + 1);
            if (g.length() > 1 && g.charAt(0) == '0') throw new Invalid("no leading zeros in the address");
            o[i] = Integer.parseInt(g);
            if (o[i] > 255) throw new Invalid("bad address");
        }
        boolean ok = o[0] == 10
                || (o[0] == 172 && o[1] >= 16 && o[1] <= 31)
                || (o[0] == 192 && o[1] == 168)
                || (o[0] == 100 && o[1] >= 64 && o[1] <= 127);
        if (!ok) throw new Invalid("h must be a home-network (10/8, 172.16/12, 192.168/16) or tailnet address");
    }

    private static byte[] b64url16(String s) {
        byte[] out = new byte[16];
        int bits = 0, acc = 0, n = 0;
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            int v = c >= 'A' && c <= 'Z' ? c - 'A' : c >= 'a' && c <= 'z' ? c - 'a' + 26 : c >= '0' && c <= '9' ? c - '0' + 52 : c == '-' ? 62 : 63;
            acc = (acc << 6) | v;
            bits += 6;
            if (bits >= 8) {
                bits -= 8;
                if (n < 16) out[n++] = (byte) (acc >> bits);
                acc &= (1 << bits) - 1;
            }
        }
        return out;
    }

    private static String safe(String s) {
        return s.length() > 16 ? s.substring(0, 16) + "..." : s;
    }
}
