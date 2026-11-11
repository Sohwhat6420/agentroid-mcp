package dev.agentroid.helper.link.net;

import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.UnknownHostException;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * Task 2.12: the link dials ONLY into the tailnet, 100.64.0.0/10 (WireGuard encrypts the plain ws://). This is
 * stricter than the 2.5 HubSpec, which also allowed loopback for `adb reverse` bring-up: the 2.12 link refuses
 * loopback, LAN and public addresses. A MagicDNS name (*.ts.net) is accepted as a name, but every address it resolves
 * to is filtered through {@link #onlyTailnet} at dial time (OkHttpTransport's Dns), so a spoofed answer cannot steer
 * the link off the tailnet. IPv6 (including Tailscale's fd7a:115c:a1e0::/48) is refused: v1 is IPv4 only.
 */
public final class TailnetGuard {
    private TailnetGuard() { }

    private static final Pattern IPV4 = Pattern.compile("(\\d{1,3})\\.(\\d{1,3})\\.(\\d{1,3})\\.(\\d{1,3})");

    public static final class Refused extends Exception {
        public Refused(String m) { super(m); }
    }

    /** 100.64.0.0/10: first octet 100, second octet 64..127. */
    public static boolean isTailnet(InetAddress a) {
        if (!(a instanceof Inet4Address)) return false;
        byte[] b = a.getAddress();
        return (b[0] & 0xff) == 100 && ((b[1] & 0xff) & 0xc0) == 64;
    }

    /** Parses a dotted-quad literal without any DNS lookup; null if it is not one (or has octal-looking parts). */
    static int[] literal(String host) {
        Matcher m = IPV4.matcher(host);
        if (!m.matches()) return null;
        int[] o = new int[4];
        for (int i = 0; i < 4; i++) {
            String p = m.group(i + 1);
            if (p.length() > 1 && p.charAt(0) == '0') return null;
            o[i] = Integer.parseInt(p);
            if (o[i] > 255) return null;
        }
        return o;
    }

    /** Checks the host part of the hub address before anything is dialled. */
    public static void checkHost(String host) throws Refused {
        if (host == null || host.isEmpty()) throw new Refused("hub host is missing");
        String h = host.toLowerCase(Locale.ROOT);
        int[] o = literal(h);
        if (o != null) {
            if (o[0] == 100 && (o[1] & 0xc0) == 64) return;
            throw new Refused("hub address is not in the tailnet range 100.64.0.0/10");
        }
        if (h.matches("[a-z0-9-]+(\\.[a-z0-9-]+)*\\.ts\\.net") && h.length() <= 253) return;
        throw new Refused("hub host must be a 100.64.0.0/10 address or a *.ts.net name");
    }

    /** Keeps only tailnet addresses; none left is a refusal (the dial fails, nothing leaves the tailnet). */
    public static List<InetAddress> onlyTailnet(String host, List<InetAddress> resolved) throws UnknownHostException {
        List<InetAddress> out = new ArrayList<InetAddress>();
        if (resolved != null) for (InetAddress a : resolved) if (isTailnet(a)) out.add(a);
        if (out.isEmpty()) throw new UnknownHostException("refused: " + host + " does not resolve into 100.64.0.0/10");
        return out;
    }
}
