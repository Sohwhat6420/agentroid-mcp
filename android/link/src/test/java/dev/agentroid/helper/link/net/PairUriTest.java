package dev.agentroid.helper.link.net;

/** The pairing-QR parser (ship/NO-ADB-PLAN.md stage 4): a QR is attacker-controllable, so everything odd is refused. */
public class PairUriTest {
    private static final long NOW = 1_800_000_000L;
    private static final String FP = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    private static final String K = "AAECAwQFBgcICQoLDA0ODw"; // bytes 0..15

    private static String rep(String c, int n) {
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < n; i++) b.append(c);
        return b.toString();
    }

    private static String good(String extra) {
        return "agentroid://pair?h=192.168.1.20:8765&fp=" + FP + "&k=" + K + "&e=" + (NOW + 300) + extra;
    }

    public void testAGoodQrParses() throws Exception {
        PairUri p = PairUri.parse(good("&n=Alex-PC"), NOW);
        A.eq("192.168.1.20", p.host, "host");
        A.eq(8765L, p.port, "port");
        A.eq(NOW + 300, p.expiresAt, "expiry");
        A.eq("Alex-PC", p.laptop, "laptop");
        for (int i = 0; i < 16; i++) A.eq((long) i, p.code[i], "code byte " + i);
        A.eq(0x00L, p.certSha256[0], "pin byte 0");
        A.eq((long) (byte) 0xff, p.certSha256[31], "pin byte 31");
        A.ok(PairUri.parse(good(""), NOW).laptop == null, "n is optional");
        A.eq("Alex PC", PairUri.parse(good("&n=Alex%20PC"), NOW).laptop, "n is percent-decoded");
    }

    public void testEveryPrivateRangeAndTheTailnetAreAccepted() throws Exception {
        for (String h : new String[] {"10.0.0.5", "172.16.0.1", "172.31.255.254", "192.168.0.1", "100.64.0.1", "100.127.1.1"}) {
            A.eq(h, PairUri.parse(good("").replace("192.168.1.20", h), NOW).host, h);
        }
    }

    public void testPublicLoopbackAndNamedHubsAreRefused() {
        for (final String h : new String[] {"8.8.8.8", "127.0.0.1", "0.0.0.0", "169.254.1.1", "172.32.0.1", "172.15.0.1",
                "100.128.0.1", "192.169.0.1", "255.255.255.255", "hub.local", "evil.example", "192.168.1.256",
                "192.168.01.20", "[::1]", "192.168.1"}) {
            A.throwsA(PairUri.Invalid.class, new A.Body() {
                public void run() throws Exception { PairUri.parse(good("").replace("192.168.1.20", h), NOW); }
            }, "refuses hub " + h);
        }
    }

    public void testExpiredOrFarFutureCodesAreRefused() {
        final String[] bad = {
            "agentroid://pair?h=192.168.1.20:8765&fp=" + FP + "&k=" + K + "&e=" + NOW,
            "agentroid://pair?h=192.168.1.20:8765&fp=" + FP + "&k=" + K + "&e=" + (NOW - 1),
            "agentroid://pair?h=192.168.1.20:8765&fp=" + FP + "&k=" + K + "&e=" + (NOW + PairUri.MAX_AHEAD_S + 1),
        };
        for (final String s : bad) {
            A.throwsA(PairUri.Invalid.class, new A.Body() { public void run() throws Exception { PairUri.parse(s, NOW); } }, s);
        }
    }

    public void testMalformedQrsAreRefused() {
        final String[] bad = {
            null, "", "https://pair?h=1", "AGENTROID://pair?" + good("").split("\\?", 2)[1],
            "agentroid://evil?" + good("").split("\\?", 2)[1],
            "agentroid://user@pair?" + good("").split("\\?", 2)[1],
            "agentroid://pair/x?" + good("").split("\\?", 2)[1],
            good("#frag"),
            good("&x=1"),                                  // unknown parameter
            good("&k=" + K),                               // repeated parameter
            good("").replace("&k=" + K, ""),               // missing k
            good("").replace("fp=" + FP, "fp=" + FP.toUpperCase()),
            good("").replace("fp=" + FP, "fp=" + FP.substring(2)),
            good("").replace("k=" + K, "k=" + K + "A"),
            good("").replace("k=" + K, "k=AAECAwQFBgcICQoLDA0ODx"), // non-zero trailing bits
            good("").replace("k=" + K, "k=AAECAwQFBgcICQoLDA0ODw=="),
            good("").replace(":8765", ""),
            good("").replace(":8765", ":0"),
            good("").replace(":8765", ":65536"),
            good("").replace(":8765", ":08765"),
            good("&n=" + rep("x", 65)),
            good("&n=bad%0Aname"),
            good("&n=%3Cscript%3E"),
            good("&n=%ZZ"),
            "agentroid://pair?" + rep("a", PairUri.MAX_LEN),
        };
        for (final String s : bad) {
            A.throwsA(PairUri.Invalid.class, new A.Body() { public void run() throws Exception { PairUri.parse(s, NOW); } },
                    "refuses " + (s == null ? "null" : s.length() > 80 ? s.substring(0, 80) + "..." : s));
        }
    }
}
