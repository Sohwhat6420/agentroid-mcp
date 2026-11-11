package dev.agentroid.helper.link.net;

import java.io.File;
import java.io.Serializable;
import java.lang.reflect.Method;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.security.Key;
import java.security.KeyPair;
import java.security.PrivateKey;

import dev.agentroid.helper.core.LinkSession;

/** Identity: the challenge round trip, wrong key and stale epoch -> needs_repair, the private key never serialised. */
public class IdentityTest {
    static final String N1 = LinkCrypto.b64(new byte[32]);
    static final String N2 = LinkCrypto.b64(filled(32, 7));

    static byte[] filled(int n, int v) {
        byte[] b = new byte[n];
        java.util.Arrays.fill(b, (byte) v);
        return b;
    }

    public void testChallengeSignAndVerifyRoundTrip() throws Exception {
        SoftwareIdentity phone = new SoftwareIdentity(1), other = new SoftwareIdentity(2);
        String hubKid = LinkCrypto.keyId(new SoftwareIdentity(3).publicKeySpki());
        byte[] msg = LinkCrypto.phoneResponseMessage(N1, N2, 5, "SER1", hubKid);
        byte[] sig = phone.sign(msg);
        A.ok(LinkCrypto.verify(phone.publicKeySpki(), msg, sig), "verifies with the phone's public key");
        A.ok(!LinkCrypto.verify(other.publicKeySpki(), msg, sig), "does not verify with another key");
        A.ok(!LinkCrypto.verify(phone.publicKeySpki(), LinkCrypto.phoneResponseMessage(N2, N2, 5, "SER1", hubKid), sig), "nonce is covered");
        A.ok(!LinkCrypto.verify(phone.publicKeySpki(), LinkCrypto.phoneResponseMessage(N1, N1, 5, "SER1", hubKid), sig), "phone nonce is covered");
        A.ok(!LinkCrypto.verify(phone.publicKeySpki(), LinkCrypto.phoneResponseMessage(N1, N2, 6, "SER1", hubKid), sig), "epoch is covered");
        A.ok(!LinkCrypto.verify(phone.publicKeySpki(), LinkCrypto.phoneResponseMessage(N1, N2, 5, "SER2", hubKid), sig), "serial is covered");
        A.ok(!LinkCrypto.verify(phone.publicKeySpki(), LinkCrypto.hubChallengeMessage(N1, N2, 5, "SER1", hubKid), sig),
                "domain separation: a phone-response signature is not a hub-challenge signature");
        A.ok(!LinkCrypto.verify(phone.publicKeySpki(), msg, new byte[70]), "garbage signature");
        A.ok(!LinkCrypto.verify(new byte[] {1, 2, 3}, msg, sig), "garbage key");
        String text = new String(msg, StandardCharsets.UTF_8);
        A.eq("agentroid-link/v1/phone-response\nnonce=" + N1 + "\nphone_nonce=" + N2 + "\nepoch=5\nserial=SER1\nhub_key_id=" + hubKid, text,
                "the signed message layout written in link.json");
    }

    public void testFullHandshakeAuthenticatesBothWays() {
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 4);
        Phone p = new Phone(clock, hub, 21);
        LinkTest.readyUp(clock, p);
        A.eq(LinkSession.State.READY, p.session.state(), "session ready");
        A.eq(1, hub.readies, "the hub verified the phone signature once");
        A.eq(4, p.trust.maxEpoch(), "epoch remembered");
    }

    private static Phone failing(FakeClock clock, FakeHub hub, Phone p, final String reason) {
        p.start();
        clock.runUntil(new FakeClock.Until() { public boolean done() { return p.session.state() == LinkSession.State.NEEDS_REPAIR; } }, 60000, reason);
        clock.runFor(10);
        A.eq(reason, p.session.failure(), "repair reason");
        String fix = LinkProto.PHONE_FIX.containsKey(reason) ? LinkProto.PHONE_FIX.get(reason) : LinkProto.HUB_FIX.get(reason);
        A.ok(fix != null && fix.length() > 40, "a real fix text for " + reason);
        A.eq(fix, p.session.fix(), "the session carries the exact fix text");
        A.ok(p.client.phase() != LinkClient.Phase.READY, "not ready");
        A.eq(0, hub.readies, "never ready");
        return p;
    }

    public void testWrongHubKeyGivesNeedsRepair() {
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 4);
        Phone p = new Phone(clock, hub, 21);
        hub.signAs = new SoftwareIdentity(99); // another machine at the hub address
        failing(clock, hub, p, "bad_hub_signature");
        A.eq(LinkProto.PHONE_FIX.get("bad_hub_signature"), hub.repairs.get(0).get("fix"), "exact fix on the wire");
        A.eq("phone", hub.repairs.get(0).get("from"), "from phone");
    }

    public void testHubKeyChangedSincePairingGivesNeedsRepair() {
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 4);
        Phone p = new Phone(clock, hub, 21);
        p.trust.hubKey = new SoftwareIdentity(98).publicKeySpki(); // paired with a different hub key
        failing(clock, hub, p, "bad_hub_signature");
    }

    public void testUnpairedPhoneGivesHubKeyMissing() {
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 4);
        Phone p = new Phone(clock, hub, 21);
        p.trust.hubKey = null;
        failing(clock, hub, p, "hub_key_missing");
    }

    public void testSerialMismatchGivesNeedsRepair() {
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 4);
        Phone p = new Phone(clock, hub, 21);
        hub.serial = "SOMEOTHERPHONE";
        failing(clock, hub, p, "serial_mismatch");
    }

    public void testStaleEpochGivesNeedsRepair() {
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 4);
        Phone p = new Phone(clock, hub, 21);
        p.trust.maxEpoch = 9;
        failing(clock, hub, p, "stale_epoch");
    }

    public void testReplayedChallengeGivesNeedsRepair() {
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 4);
        Phone p = new Phone(clock, hub, 21);
        hub.forcePhoneNonce = N2; // a validly signed challenge, but for some other hello
        failing(clock, hub, p, "bad_challenge");
    }

    public void testHubRejectsThePhoneSignature() {
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 4);
        Phone p = new Phone(clock, hub, 21);
        hub.phoneKey = new SoftwareIdentity(97).publicKeySpki();
        failing(clock, hub, p, "bad_phone_signature");
    }

    public void testBrokenKeystoreKeyGivesKeyUnavailable() {
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 4);
        Phone p = new Phone(clock, hub, 21);
        p.key.breakKey();
        failing(clock, hub, p, "key_unavailable");
    }

    public void testRevokedKeyGivesNeedsRepairWithin15s() {
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 4);
        final Phone p = new Phone(clock, hub, 21);
        LinkTest.readyUp(clock, p);
        hub.revoked = true;
        hub.drop(); // the hub closes the revoked phone's socket; the next challenge_response gets key_revoked
        long t0 = clock.nowMs();
        clock.runUntil(new FakeClock.Until() { public boolean done() { return p.session.state() == LinkSession.State.NEEDS_REPAIR; } }, 15000, "key_revoked");
        A.ok(clock.nowMs() - t0 <= 15000, "needs_repair within 15 s");
        A.eq(LinkProto.HUB_FIX.get("key_revoked"), p.session.fix(), "exact hub fix text");
    }

    public void testNeedsRepairIsStickyUntilAChallengePasses() {
        final FakeClock clock = new FakeClock();
        final FakeHub hub = LinkTest.hub(clock, 4);
        final Phone p = new Phone(clock, hub, 21);
        hub.signAs = new SoftwareIdentity(99);
        failing(clock, hub, p, "bad_hub_signature");
        clock.runFor(5000);
        A.eq(LinkSession.State.NEEDS_REPAIR, p.session.state(), "still needs_repair across redials");
        A.ok(!hub.phoneFrames("hello").isEmpty() && "needs_repair".equals(hub.phoneFrames("hello").get(hub.phoneFrames("hello").size() - 1).get("state")),
                "the hello tells the hub");
        hub.signAs = null; // the laptop was re-paired
        clock.runUntil(new FakeClock.Until() { public boolean done() { return p.client.phase() == LinkClient.Phase.READY; } }, 60000, "ready after repair");
        A.eq(LinkSession.State.READY, p.session.state(), "cleared by a passing challenge");
    }

    public void testPrivateKeyIsNeverSerialised() throws Exception {
        // 1. the interface has no way out for the private key
        for (Method m : Identity.class.getMethods()) {
            Class<?> r = m.getReturnType();
            A.ok(!Key.class.isAssignableFrom(r) || r == java.security.PublicKey.class, "Identity." + m.getName() + " returns a key type");
            A.ok(!PrivateKey.class.isAssignableFrom(r) && r != KeyPair.class, "Identity." + m.getName() + " returns private material");
        }
        A.ok(!Serializable.class.isAssignableFrom(SoftwareIdentity.class), "the test identity is not Serializable");
        A.ok(!Serializable.class.isAssignableFrom(Identity.class), "Identity is not Serializable");

        // 2. a whole session (handshake, 50 requests, heartbeats, a drop and a resume) never carries the private key
        FakeClock clock = new FakeClock();
        FakeHub hub = LinkTest.hub(clock, 4);
        Phone p = new Phone(clock, hub, 21);
        LinkTest.readyUp(clock, p);
        for (int i = 0; i < 50; i++) hub.enqueue("k" + i, "act.count", "{\"n\":" + i + "}");
        hub.pumpNow();
        clock.runFor(70000);
        hub.drop();
        clock.runFor(70000);
        StringBuilder all = new StringBuilder();
        for (String s : hub.sent) all.append(s).append('\n');
        all.append(DumpText.of(p.key));
        String out = all.toString();
        byte[] pk8 = p.key.privateEncodedForLeakCheck();
        byte[] s = p.key.privateScalarForLeakCheck().toByteArray();
        String[] needles = {LinkCrypto.b64(pk8), hex(pk8), LinkCrypto.b64(s), hex(s), p.key.privateScalarForLeakCheck().toString(),
                LinkCrypto.b64(java.util.Arrays.copyOfRange(pk8, pk8.length - 40, pk8.length - 8))};
        for (String n : needles) A.ok(!out.contains(n), "private key material found in phone output");
        A.ok(out.contains(LinkCrypto.b64(p.key.publicKeySpki())), "the public key is there (dump), so the search is looking at real output");

        // 3. the dump prints exactly four lines, public material only
        String[] lines = DumpText.of(p.key).split("\n");
        A.eq(4, lines.length, "dump lines");
        A.eq("agentroid-link v1", lines[0], "header");
        A.eq("alg=SHA256withECDSA", lines[1], "alg");
        A.eq("public_key=" + LinkCrypto.b64(p.key.publicKeySpki()), lines[2], "public key");
        A.eq("key_id=" + LinkCrypto.keyId(p.key.publicKeySpki()), lines[3], "key id");

        // 4. the AndroidKeyStore implementation (Android-only, so read as source): non-exportable, sign-only
        String ks = source("link/src/main/java/dev/agentroid/helper/link/KeystoreIdentity.java");
        A.ok(ks.contains("\"AndroidKeyStore\""), "uses AndroidKeyStore");
        A.ok(ks.contains("KeyProperties.PURPOSE_SIGN") && !ks.contains("PURPOSE_ENCRYPT") && !ks.contains("PURPOSE_DECRYPT"), "sign-only purpose");
        A.ok(ks.contains("\"secp256r1\"") && ks.contains("KeyProperties.DIGEST_SHA256"), "P-256 with SHA-256");
        A.ok(!ks.contains("Serializable"), "not Serializable");
        A.ok(!ks.matches("(?s).*PrivateKey[^;]*getEncoded.*") && !ks.matches("(?s).*getPrivate\\(\\)\\s*\\.\\s*getEncoded.*"),
                "never encodes the private key (an AndroidKeyStore private key has no encoding anyway)");
        A.ok(ks.contains("getCertificate(ALIAS)") && ks.contains("getPublicKey().getEncoded()"), "the public key comes from the certificate");
    }

    static String hex(byte[] b) {
        StringBuilder s = new StringBuilder();
        for (byte x : b) s.append(String.format("%02x", x & 0xff));
        return s.toString();
    }

    static String source(String rel) throws Exception {
        File f = new File(System.getProperty("helper.root", "android"), rel);
        return new String(Files.readAllBytes(f.toPath()), StandardCharsets.UTF_8);
    }
}
