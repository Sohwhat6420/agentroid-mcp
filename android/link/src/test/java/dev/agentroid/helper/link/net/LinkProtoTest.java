package dev.agentroid.helper.link.net;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;

import dev.agentroid.helper.core.Json;

/**
 * LinkProto against schemas/link.json (the fix texts and reason lists must be identical), the strict parser, and
 * the frame dump: a scripted session that makes every frame type, written to out/link-frames/ so test_schemas.py can
 * validate what this code really sends against link.json and verify both signatures with an independent (Python)
 * implementation of the signed-message layout.
 */
public class LinkProtoTest {
    static File root() { return new File(System.getProperty("helper.root", "android")); }

    @SuppressWarnings("unchecked")
    static Map<String, Object> schema() throws Exception {
        File f = new File(root(), "../schemas/link.json");
        return (Map<String, Object>) Json.parse(new String(Files.readAllBytes(f.toPath()), StandardCharsets.UTF_8), 64);
    }

    @SuppressWarnings("unchecked")
    public void testFixTextsAndReasonsMatchLinkJson() throws Exception {
        Map<String, Object> defs = (Map<String, Object>) schema().get("$defs");
        List<Object> pairs = (List<Object>) ((Map<String, Object>) defs.get("needs_repair")).get("oneOf");
        Map<String, String> phone = new LinkedHashMap<String, String>(), hub = new LinkedHashMap<String, String>();
        for (Object o : pairs) {
            Map<String, Object> props = (Map<String, Object>) ((Map<String, Object>) o).get("properties");
            String from = (String) ((Map<String, Object>) props.get("from")).get("const");
            String reason = (String) ((Map<String, Object>) props.get("reason")).get("const");
            String fix = (String) ((Map<String, Object>) props.get("fix")).get("const");
            (from.equals("phone") ? phone : hub).put(reason, fix);
        }
        A.eq(LinkProto.PHONE_FIX, phone, "phone-side fix texts identical to link.json");
        A.eq(LinkProto.HUB_FIX, hub, "hub-side fix texts identical to link.json");
        Set<String> all = new HashSet<String>(phone.keySet());
        all.addAll(hub.keySet());
        A.eq(all, new HashSet<Object>((List<Object>) ((Map<String, Object>) defs.get("repair_reason")).get("enum")), "repair_reason enum");
        Map<String, Object> refused = (Map<String, Object>) ((Map<String, Object>) defs.get("refused")).get("properties");
        Set<Object> xs = new HashSet<Object>((List<Object>) ((Map<String, Object>) refused.get("reason")).get("enum"));
        A.eq(new HashSet<Object>(java.util.Arrays.asList("stale_epoch", "epoch_ahead", "not_ready", "out_of_order", "bad_request")), xs, "refusal reasons");
        Map<String, Object> ub = (Map<String, Object>) ((Map<String, Object>) defs.get("untrusted_bssid")).get("properties");
        A.eq(LinkProto.BSSID_REASONS, new HashSet<Object>((List<Object>) ((Map<String, Object>) ub.get("reason")).get("enum")), "bssid reasons");
        A.eq(LinkCrypto.HUB_DOMAIN, "agentroid-link/v1/hub-challenge", "hub domain");
        A.ok(((String) ((Map<String, Object>) defs.get("challenge")).get("description")).contains(LinkCrypto.HUB_DOMAIN), "hub domain written in link.json");
        A.ok(((String) ((Map<String, Object>) defs.get("challenge_response")).get("description")).contains(LinkCrypto.PHONE_DOMAIN), "phone domain written in link.json");
        A.eq(Long.valueOf(LinkClient.HEARTBEAT_ACTIVE_MS / 1000), ((List<Object>) ((Map<String, Object>) ((Map<String, Object>) ((Map<String, Object>) defs.get("heartbeat"))
                .get("properties")).get("period_s")).get("enum")).get(0), "15 s in the schema");
    }

    public void testParserIsStrict() {
        String good = "{\"v\":1,\"t\":\"ack\",\"epoch\":3,\"ack\":0}";
        try {
            LinkProto.parseFromHub(good);
        } catch (LinkProto.BadFrame e) {
            throw new AssertionError("good ack refused: " + e.getMessage());
        }
        String[] bad = {
            "", "[]", "{}", "{\"v\":2,\"t\":\"ack\",\"epoch\":3,\"ack\":0}", "{\"v\":1,\"t\":\"ack\",\"epoch\":3,\"ack\":0,\"x\":1}",
            "{\"v\":1,\"t\":\"ack\",\"epoch\":0,\"ack\":0}", "{\"v\":1,\"t\":\"ack\",\"epoch\":3,\"ack\":-1}", "{\"v\":1,\"t\":\"hello\"}",
            "{\"v\":1,\"t\":\"request\",\"epoch\":3,\"seq\":1,\"id\":\"a\",\"rpc\":{\"jsonrpc\":\"2.0\",\"id\":\"b\",\"method\":\"ping\"}}",
            "{\"v\":1,\"t\":\"request\",\"epoch\":3,\"seq\":0,\"id\":\"a\",\"rpc\":{\"jsonrpc\":\"2.0\",\"id\":\"a\",\"method\":\"ping\"}}",
            "{\"v\":1,\"t\":\"request\",\"epoch\":3,\"seq\":1,\"id\":\"a b\",\"rpc\":{\"jsonrpc\":\"2.0\",\"id\":\"a b\",\"method\":\"ping\"}}",
            "{\"v\":1,\"t\":\"request\",\"epoch\":1.5,\"seq\":1,\"id\":\"a\",\"rpc\":{}}",
            "{\"v\":1,\"t\":\"needs_repair\",\"from\":\"phone\",\"reason\":\"key_revoked\",\"fix\":\"x\"}",
            "{\"v\":1,\"t\":\"needs_repair\",\"from\":\"hub\",\"reason\":\"made_up\",\"fix\":\"x\"}",
        };
        for (String b : bad) {
            try {
                LinkProto.parseFromHub(b);
            } catch (LinkProto.BadFrame e) {
                continue;
            }
            throw new AssertionError("should refuse: " + b);
        }
    }

    /** The hub's segment frame is read and dispatched as a runner.segment request. */
    @SuppressWarnings("unchecked")
    public void testSegmentFrameIsReadAsARunnerSegmentRequest() throws Exception {
        String env = "{\"segment\":{\"v\":1,\"segment_id\":\"seg-1\"},\"signature\":{\"alg\":\"ECDSA-P256-SHA256\",\"key_id\":\"k\",\"value\":\"AAAA\"}}";
        String good = "{\"v\":1,\"t\":\"segment\",\"epoch\":4,\"seq\":2,\"id\":\"seg-1\",\"segment\":" + env + "}";
        Map<String, Object> m = LinkProto.parseFromHub(good);
        Map<String, Object> req = LinkProto.segmentAsRequest(m);
        A.eq(LinkProto.REQUEST, req.get("t"), "answered like a request");
        A.eq(Long.valueOf(4), req.get("epoch"), "same epoch (fenced like a request)");
        A.eq(Long.valueOf(2), req.get("seq"), "same seq");
        A.eq("seg-1", req.get("id"), "same id");
        A.ok(!req.containsKey("segment"), "no segment field left on the request");
        Map<String, Object> rpc = (Map<String, Object>) req.get("rpc");
        A.eq("runner.segment", rpc.get("method"), "dispatched to runner.segment");
        A.eq("seg-1", rpc.get("id"), "rpc.id equals id");
        A.eq(Json.parse(env), rpc.get("params"), "the signed segment unchanged as params");
        String[] bad = {
            "{\"v\":1,\"t\":\"segment\",\"epoch\":4,\"seq\":0,\"id\":\"seg-1\",\"segment\":" + env + "}",
            "{\"v\":1,\"t\":\"segment\",\"epoch\":4,\"seq\":2,\"id\":\"seg 1\",\"segment\":" + env + "}",
            "{\"v\":1,\"t\":\"segment\",\"epoch\":4,\"seq\":2,\"id\":\"seg-1\",\"segment\":{\"segment\":{}}}",
            "{\"v\":1,\"t\":\"segment\",\"epoch\":4,\"seq\":2,\"id\":\"seg-1\",\"segment\":{\"segment\":{},\"signature\":{},\"x\":1}}",
            "{\"v\":1,\"t\":\"segment\",\"epoch\":4,\"seq\":2,\"id\":\"seg-1\",\"segment\":[]}",
            "{\"v\":1,\"t\":\"segment\",\"epoch\":4,\"seq\":2,\"id\":\"seg-1\",\"segment\":" + env + ",\"rpc\":{}}",
            "{\"v\":1,\"t\":\"segment\",\"epoch\":0,\"seq\":2,\"id\":\"seg-1\",\"segment\":" + env + "}",
        };
        for (String b : bad) {
            try {
                LinkProto.parseFromHub(b);
            } catch (LinkProto.BadFrame e) {
                continue;
            }
            throw new AssertionError("should refuse: " + b);
        }
    }

    public void testNonceIsAtLeast32Bytes() {
        A.ok(!LinkCrypto.nonceOk(LinkCrypto.b64(new byte[31])), "31 bytes refused");
        A.ok(LinkCrypto.nonceOk(LinkCrypto.b64(new byte[32])), "32 bytes accepted");
        A.ok(LinkCrypto.nonceOk(LinkCrypto.b64(new byte[64])), "64 bytes accepted");
        A.ok(!LinkCrypto.nonceOk(LinkCrypto.b64(new byte[65])), "65 bytes refused");
        A.ok(!LinkCrypto.nonceOk(LinkCrypto.b64(new byte[32]).replace("=", "")), "unpadded refused");
        A.ok(!LinkCrypto.nonceOk("AQ" + LinkCrypto.b64(new byte[32]).substring(2, 43) + "B="), "non-canonical trailing bits refused");
        java.security.SecureRandom r = new java.security.SecureRandom();
        A.eq(44, LinkCrypto.newNonce(r).length(), "a fresh nonce is 32 bytes (44 base64 characters)");
        A.ok(!LinkCrypto.newNonce(r).equals(LinkCrypto.newNonce(r)), "fresh nonces differ");
    }

    /** Writes one example of every frame this code sends and the fake hub sends, plus the signing transcript. */
    @SuppressWarnings("unchecked")
    public void testWriteFrameDumpForSchemaCheck() throws Exception {
        File dir = new File(root(), "out/link-frames");
        dir.mkdirs();
        File[] old = dir.listFiles();
        if (old != null) for (File f : old) f.delete();

        final FakeClock clock = new FakeClock();
        final FakeHub hub = LinkTest.hub(clock, 5);
        final Phone p = new Phone(clock, hub, 21);
        p.beacon.put("adb_port", Long.valueOf(40111));
        p.beacon.put("adb_port_source", "nsd");
        p.beacon.put("wifi_adb", Boolean.TRUE);
        p.beacon.put("a11y_enabled", Boolean.TRUE);
        p.beacon.put("keeper", "ok");
        p.beacon.put("bssid_trusted", Boolean.TRUE);
        LinkTest.readyUp(clock, p);
        hub.enqueue("ok-1", "act.count", "{\"n\":1}");
        hub.enqueue("ok-1", "act.count", "{\"n\":1}");          // replayed
        hub.enqueue("err-1", "no.such_method", null);           // an error response
        hub.pumpNow();
        clock.runFor(100);
        hub.raw(FakeHub.requestFrame(4, 99, "stale-1", "act.count", "{\"n\":2}"));
        hub.raw(FakeHub.requestFrame(6, 99, "ahead-1", "act.count", "{\"n\":2}"));
        hub.raw(FakeHub.requestFrame(5, 1, "dup-1", "act.count", "{\"n\":2}")); // seq 1 again -> phone acks
        hub.raw(FakeHub.requestFrame(5, 50, "gap-1", "act.count", "{\"n\":2}"));
        hub.raw("{\"v\":1,\"t\":\"request\",\"epoch\":5,\"seq\":4,\"id\":\"bad-1\",\"rpc\":{\"jsonrpc\":\"2.0\",\"id\":\"bad-1\",\"method\":\"x\"},\"extra\":1}");
        p.client.sendEvent(LinkProto.untrustedBssid("aa:bb:cc:dd:ee:01", "not_in_trusted_list", clock.wallMs()));
        p.client.sendEvent(LinkProto.untrustedBssid(null, "bssid_unreadable", clock.wallMs()));
        p.client.sendEvent(LinkProto.humanTouch(86_400_000L, 86_400_020L));
        clock.runFor(100);
        hub.drop();
        clock.runFor(5000);

        // a second phone the hub refuses (hub-side needs_repair) and one that refuses the hub (phone-side needs_repair)
        FakeClock c2 = new FakeClock();
        FakeHub h2 = LinkTest.hub(c2, 5);
        Phone p2 = new Phone(c2, h2, 22);
        h2.phoneKey = new SoftwareIdentity(77).publicKeySpki();
        p2.start();
        c2.runFor(100);
        FakeClock c3 = new FakeClock();
        FakeHub h3 = LinkTest.hub(c3, 5);
        Phone p3 = new Phone(c3, h3, 23);
        p3.trust.maxEpoch = 9;
        p3.start();
        c3.runFor(100);
        clock.runFor(0);
        // a request before ready
        FakeClock c4 = new FakeClock();
        FakeHub h4 = LinkTest.hub(c4, 5);
        Phone p4 = new Phone(c4, h4, 24);
        h4.phoneKey = null;
        p4.start();
        c4.runFor(2);
        h4.raw(FakeHub.requestFrame(5, 1, "early-1", "act.count", null));
        c4.runFor(50);

        Set<String> seen = new HashSet<String>();
        int n = 0;
        for (FakeHub h : new FakeHub[] {hub, h2, h3, h4}) {
            for (String[] pair : new String[][] {{"phone", null}, {"hub", null}}) {
                List<String> frames = pair[0].equals("phone") ? h.sent : h.hubSent;
                for (String s : frames) {
                    Map<String, Object> m = (Map<String, Object>) Json.parse(s);
                    String key = pair[0] + "-" + m.get("t") + (m.containsKey("reason") ? "-" + m.get("reason") : "")
                            + (Boolean.TRUE.equals(m.get("replayed")) ? "-replayed" : "")
                            + ("response".equals(m.get("t")) && ((Map<String, Object>) m.get("rpc")).containsKey("error") ? "-error" : "");
                    if (!seen.add(key)) continue;
                    Files.write(new File(dir, String.format("%02d-%s.json", n++, key)).toPath(), s.getBytes(StandardCharsets.UTF_8));
                }
            }
        }
        String[] need = {"phone-hello", "phone-challenge_response", "phone-heartbeat", "phone-response", "phone-response-replayed",
            "phone-response-error", "phone-ack", "phone-resume", "phone-refused-stale_epoch", "phone-refused-epoch_ahead",
            "phone-refused-out_of_order", "phone-refused-bad_request", "phone-refused-not_ready", "phone-untrusted_bssid-not_in_trusted_list",
            "phone-untrusted_bssid-bssid_unreadable", "phone-human_touch", "phone-needs_repair-stale_epoch", "hub-challenge", "hub-ready", "hub-request", "hub-ack",
            "hub-resume", "hub-needs_repair-bad_phone_signature"};
        for (String k : need) A.ok(seen.contains(k), "frame dump has " + k + " (have " + seen + ")");

        // signing transcript for the independent Python check
        Map<String, Object> hello = null, challenge = null, response = null;
        for (String s : hub.sent) {
            Map<String, Object> m = (Map<String, Object>) Json.parse(s);
            if (hello == null && "hello".equals(m.get("t"))) hello = m;
            if (response == null && "challenge_response".equals(m.get("t"))) response = m;
        }
        for (String s : hub.hubSent) {
            Map<String, Object> m = (Map<String, Object>) Json.parse(s);
            if (challenge == null && "challenge".equals(m.get("t"))) challenge = m;
        }
        Map<String, Object> tr = new LinkedHashMap<String, Object>();
        tr.put("note", "Made by LinkProtoTest from a real LinkClient session against FakeHub. test_schemas.py verifies both signatures.");
        tr.put("hub_public_key", LinkCrypto.b64(hub.hubSpki()));
        tr.put("phone_public_key", LinkCrypto.b64(p.key.publicKeySpki()));
        tr.put("serial", LinkTest.SERIAL);
        tr.put("hello", hello);
        tr.put("challenge", challenge);
        tr.put("challenge_response", response);
        Files.write(new File(dir, "transcript.json").toPath(), Json.write(tr).getBytes(StandardCharsets.UTF_8));
        System.out.println("  frame dump: " + n + " distinct frames and transcript.json in out/link-frames");
    }
}
