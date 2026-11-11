package dev.agentroid.helper.core;

import java.util.Random;

/** HubSpec (where we dial), the notification wording, and the reconnect backoff. */
public class SmallPartsTest {

    private static void accepts(String hub, String host, int port) throws Exception {
        HubSpec h = HubSpec.parse(hub, null);
        T.eq(host, h.host, hub + " host");
        T.eq(Integer.valueOf(port), Integer.valueOf(h.port), hub + " port");
    }

    private static void refuses(String hub) {
        T.throwsA(HubSpec.Invalid.class, () -> HubSpec.parse(hub, null), "should refuse " + hub);
    }

    public void testHubSpecAcceptsTailnetAndLoopbackOnly() throws Exception {
        accepts("100.64.0.1", "100.64.0.1", 8765);
        accepts("100.127.255.254:9000", "100.127.255.254", 9000);
        accepts("127.0.0.1:8765", "127.0.0.1", 8765);
        accepts("localhost", "localhost", 8765);
        accepts("Laptop.Tail1234.TS.net:8765", "laptop.tail1234.ts.net", 8765);
        T.eq("ws://100.64.0.1:8765/helper", HubSpec.parse("100.64.0.1", null).url(), "url");
    }

    public void testHubSpecRefusesEverythingElse() {
        refuses("192.168.1.20");
        refuses("10.0.0.5:8765");
        refuses("172.16.0.1");
        refuses("100.63.255.255");
        refuses("100.128.0.1");
        refuses("8.8.8.8");
        refuses("example.com");
        refuses("ts.net");
        refuses(".ts.net");
        refuses("a..ts.net");
        refuses("evil.ts.net.example.com");
        refuses("100.064.0.1");
        refuses("100.64.0.256");
        refuses("100.64.0.1:0");
        refuses("100.64.0.1:65536");
        refuses("100.64.0.1:abc");
        refuses("100.64.0.1/path");
        refuses("user@100.64.0.1");
        refuses("[::1]:8765");
        refuses("");
        refuses(null);
    }

    public void testLaptopLabelIsSanitised() throws Exception {
        T.eq("Alex's laptop", HubSpec.parse("100.64.0.1", "Alex's laptop").laptopLabel, "plain name kept");
        T.eq("100.64.0.1", HubSpec.parse("100.64.0.1", null).laptopLabel, "default is the host");
        T.eq("100.64.0.1", HubSpec.parse("100.64.0.1", "   ").laptopLabel, "blank falls back to host");
        T.eq("LAPTOPgpj", HubSpec.parse("100.64.0.1", "LAPTOP‮\ngpj\r").laptopLabel, "bidi override and newlines stripped");
        StringBuilder long60 = new StringBuilder();
        for (int i = 0; i < 60; i++) long60.append('x');
        T.eq(Integer.valueOf(40), Integer.valueOf(HubSpec.parse("100.64.0.1", long60.toString()).laptopLabel.length()), "capped at 40");
        T.eq("café 東京", HubSpec.parse("100.64.0.1", "café 東京").laptopLabel, "unicode names kept");
    }

    public void testNotificationNamesTheLaptopInEveryState() {
        for (LinkSession.State st : LinkSession.State.values()) {
            for (boolean connected : new boolean[] {true, false}) {
                String b = NotificationText.body(st, connected, "Alex-PC");
                T.ok(b.contains("Alex-PC"), "names the laptop in " + st + "/" + connected + ": " + b);
            }
        }
        T.ok(NotificationText.body(LinkSession.State.READY, true, "L").contains("controlled by L"), "ready wording");
        T.ok(NotificationText.body(LinkSession.State.NEEDS_REPAIR, true, "L").contains("refused"), "repair wording says requests are refused");
        T.ok(NotificationText.TITLE.toLowerCase().contains("active"), "title says active");
    }

    public void testBackoffStaysWithinBoundsAndGrows() {
        Random rnd = new Random(7);
        for (int attempt = 0; attempt < 25; attempt++) {
            for (int k = 0; k < 500; k++) {
                long d = Backoff.delayMs(attempt, rnd);
                T.ok(d >= 500 && d <= Backoff.MAX_MS, "attempt " + attempt + " gave " + d);
            }
        }
        long minLate = Long.MAX_VALUE;
        for (int k = 0; k < 500; k++) minLate = Math.min(minLate, Backoff.delayMs(20, rnd));
        T.ok(minLate >= 15000, "late attempts wait at least 15 s, got " + minLate);
        long maxEarly = 0;
        for (int k = 0; k < 500; k++) maxEarly = Math.max(maxEarly, Backoff.delayMs(0, rnd));
        T.ok(maxEarly <= 1000, "first attempt at most 1 s, got " + maxEarly);
        T.ok(Backoff.delayMs(-3, rnd) <= 1000, "negative attempt treated as 0");
    }

    public void testJsonWriterRefusesNonFiniteNumbers() {
        T.throwsA(IllegalArgumentException.class, () -> Json.write(Double.valueOf(Double.NaN)), "NaN");
        T.throwsA(IllegalArgumentException.class, () -> Json.write(Double.valueOf(Double.POSITIVE_INFINITY)), "Infinity");
        T.eq("[1,2.5,\"a\\u0001\"]", Json.write(java.util.Arrays.asList(1, 2.5, "a\u0001")), "writer output");
    }
}
