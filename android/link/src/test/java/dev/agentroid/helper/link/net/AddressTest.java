package dev.agentroid.helper.link.net;

import java.net.InetAddress;
import java.net.UnknownHostException;
import java.security.KeyPairGenerator;
import java.security.spec.ECGenParameterSpec;
import java.util.Arrays;
import java.util.List;

import okhttp3.Dns;

/** A non-tailnet hub address is refused, before anything is dialled and again after DNS. */
public class AddressTest {
    static final String[] REFUSED = {
        "192.168.1.10", "10.0.0.5", "172.16.0.1", "127.0.0.1", "localhost", "8.8.8.8", "0.0.0.0", "255.255.255.255",
        "100.63.255.255", "100.128.0.0", "100.0.0.1", "101.64.0.1", "100.064.0.1", "example.com", "hub.lan", "ts.net", ".ts.net",
        "evil.ts.net.example.com", "::1", "fd7a:115c:a1e0::1", "[100.64.0.1]", "100.64.0.1.nip.io", "", "100.64.1",
    };
    static final String[] ACCEPTED = {"100.64.0.0", "100.64.0.1", "100.100.100.100", "100.127.255.255", "alexs-pc.tail1234.ts.net"};

    public void testNonTailnetHubAddressIsRefusedAndNothingIsDialled() {
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 1);
        Phone p = new Phone(clock, hub, 21);
        for (final String h : REFUSED) {
            A.throwsA(TailnetGuard.Refused.class, new A.Body() { public void run() throws Exception { p.client.start(h, 8765); } }, "refuses " + h);
        }
        clock.runFor(60000);
        A.eq(0, hub.dialTimes.size(), "nothing was dialled");
        // the guard on its own, so it is tested independently of HubSpec's own host check behind it
        for (final String h : REFUSED) {
            A.throwsA(TailnetGuard.Refused.class, new A.Body() { public void run() throws Exception { TailnetGuard.checkHost(h); } }, "checkHost refuses " + h);
        }
        for (String h : ACCEPTED) {
            try {
                TailnetGuard.checkHost(h);
            } catch (TailnetGuard.Refused e) {
                throw new AssertionError("should accept " + h + ": " + e.getMessage());
            }
        }
    }

    public void testResolvedAddressesOutsideTheTailnetAreDropped() throws Exception {
        final List<InetAddress> lan = Arrays.asList(InetAddress.getByAddress(new byte[] {(byte) 192, (byte) 168, 1, 5}));
        final List<InetAddress> mixed = Arrays.asList(InetAddress.getByAddress(new byte[] {10, 0, 0, 1}),
                InetAddress.getByAddress(new byte[] {100, 100, 1, 1}), InetAddress.getByAddress(new byte[] {127, 0, 0, 1}));
        final OkHttpTransport.TailnetDns lanDns = new OkHttpTransport.TailnetDns(new Dns() {
            public List<InetAddress> lookup(String h) { return lan; }
        });
        A.throwsA(UnknownHostException.class, new A.Body() { public void run() throws Exception { lanDns.lookup("hub.tail1.ts.net"); } },
                "a *.ts.net name that resolves into the LAN is refused");
        OkHttpTransport.TailnetDns mixedDns = new OkHttpTransport.TailnetDns(new Dns() {
            public List<InetAddress> lookup(String h) { return mixed; }
        });
        List<InetAddress> kept = mixedDns.lookup("hub.tail1.ts.net");
        A.eq(1, kept.size(), "only the tailnet address is kept");
        A.eq("100.100.1.1", kept.get(0).getHostAddress(), "kept address");
        // and the real OkHttp client is built with that Dns and no proxy
        OkHttpTransport t = new OkHttpTransport();
        A.ok(t.client().dns() instanceof OkHttpTransport.TailnetDns, "OkHttp uses the tailnet filter");
        A.eq(java.net.Proxy.NO_PROXY, t.client().proxy(), "no proxy");
    }

    public void testPairingAcceptsOnlyAPublicP256KeyAndATailnetAddress() throws Exception {
        SoftwareIdentity hub = new SoftwareIdentity(5);
        String pub = LinkCrypto.b64(hub.publicKeySpki());
        Pairing ok = Pairing.parse(pub, "100.101.102.103:8765", "POCO123", "Alex-PC");
        A.eq("100.101.102.103", ok.hub.host, "host");
        A.eq("POCO123", ok.serial, "serial");
        expectInvalid(LinkCrypto.b64(hub.privateEncodedForLeakCheck()), "100.101.102.103", "POCO123", "PRIVATE");
        KeyPairGenerator g = KeyPairGenerator.getInstance("EC");
        g.initialize(new ECGenParameterSpec("secp384r1"));
        expectInvalid(LinkCrypto.b64(g.generateKeyPair().getPublic().getEncoded()), "100.101.102.103", "POCO123", "P-256");
        KeyPairGenerator rsa = KeyPairGenerator.getInstance("RSA");
        rsa.initialize(1024);
        expectInvalid(LinkCrypto.b64(rsa.generateKeyPair().getPublic().getEncoded()), "100.101.102.103", "POCO123", "P-256");
        expectInvalid(pub, "192.168.1.20:8765", "POCO123", "refused");
        expectInvalid(pub, "127.0.0.1:8765", "POCO123", "refused");
        expectInvalid(pub, "100.101.102.103", "bad serial!", "serial");
        expectInvalid("!!" + pub, "100.101.102.103", "POCO123", "base64");
        expectInvalid(null, "100.101.102.103", "POCO123", "missing");
    }

    private static void expectInvalid(String key, String hub, String serial, String word) {
        try {
            Pairing.parse(key, hub, serial, null);
        } catch (Pairing.Invalid e) {
            A.ok(e.getMessage().contains(word), "message '" + e.getMessage() + "' mentions " + word);
            return;
        }
        throw new AssertionError("pairing should refuse " + hub + " / " + serial);
    }
}
