package dev.agentroid.helper.runner;

import dev.agentroid.helper.core.Json;

import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.Signature;
import java.security.spec.ECGenParameterSpec;
import java.time.Instant;
import java.util.ArrayList;
import java.util.Base64;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * A hub for the JVM tests: a software P-256 key that signs segment maps with Canonical (proven identical to the Rust
 * hub by SegmentFixturesTest). It signs WHATEVER it is given, including steps the real hub would refuse to build:
 * that is how the tests check that the phone does its own classification.
 */
final class TestHub {
    final KeyPair kp;
    final String serial;
    long epoch;

    TestHub(String serial, long epoch) throws Exception {
        KeyPairGenerator g = KeyPairGenerator.getInstance("EC");
        g.initialize(new ECGenParameterSpec("secp256r1"));
        this.kp = g.generateKeyPair();
        this.serial = serial;
        this.epoch = epoch;
    }

    byte[] spki() { return kp.getPublic().getEncoded(); }

    static String utc(long ms) {
        return Instant.ofEpochMilli(ms - Math.floorMod(ms, 1000L)).toString();
    }

    static Map<String, Object> sel(String key, String value) {
        Map<String, Object> m = new LinkedHashMap<String, Object>();
        m.put(key, value);
        return m;
    }

    static Map<String, Object> post(String kind, Map<String, Object> select, String text) {
        Map<String, Object> m = new LinkedHashMap<String, Object>();
        m.put("kind", kind);
        if (select != null) m.put("select", select);
        if (text != null) m.put("text", text);
        return m;
    }

    static Map<String, Object> step(String id, String kind, String pkg, String risk, Map<String, Object> select, Map<String, Object> post) {
        Map<String, Object> m = new LinkedHashMap<String, Object>();
        m.put("id", id);
        m.put("kind", kind);
        m.put("package", pkg);
        if (select != null) m.put("select", select);
        m.put("post", post);
        m.put("risk", risk);
        return m;
    }

    static final String SETTINGS = "com.android.settings";

    static Map<String, Object> read(String id) { return step(id, "read", SETTINGS, "R0", sel("text", "Battery " + id), post("none", null, null)); }

    static Map<String, Object> waitMs(String id, long ms) {
        Map<String, Object> s = step(id, "wait", SETTINGS, "R0", null, post("none", null, null));
        s.put("value", Long.valueOf(ms));
        return s;
    }

    static Map<String, Object> tap(String id) {
        return step(id, "tap", SETTINGS, "R1", sel("id", "com.android.settings:id/item_" + id), post("present", sel("text", "After " + id), null));
    }

    static Map<String, Object> scroll(String id) {
        Map<String, Object> s = step(id, "scroll", SETTINGS, "R0", null, post("present", sel("text", "Row " + id), null));
        s.put("direction", "down");
        return s;
    }

    static Map<String, Object> back(String id) { return step(id, "back", SETTINGS, "R0", null, post("absent", sel("text", "Dialog " + id), null)); }

    static Map<String, Object> toggle(String id, String key, String value) {
        Map<String, Object> s = step(id, "settings_set", SETTINGS, "R1", null, post("setting_equals", null, null));
        s.put("namespace", "system");
        s.put("key", key);
        s.put("value", value);
        s.put("scope", "job");
        s.put("lease", "lease-" + id);
        return s;
    }

    Map<String, Object> segment(String id, String classification, long expiresMs, boolean owned, List<Map<String, Object>> steps) {
        Map<String, Object> m = new LinkedHashMap<String, Object>();
        m.put("v", Long.valueOf(1));
        m.put("segment_id", id);
        m.put("run_id", "run-" + id);
        m.put("device", serial);
        m.put("epoch", Long.valueOf(epoch));
        m.put("owned", Boolean.valueOf(owned));
        m.put("classification", classification);
        m.put("expires", utc(expiresMs));
        m.put("steps", new ArrayList<Map<String, Object>>(steps));
        return m;
    }

    String sign(Map<String, Object> segment) throws Exception {
        Signature s = Signature.getInstance("SHA256withECDSA");
        s.initSign(kp.getPrivate());
        s.update(Canonical.signedBytes(segment));
        Map<String, Object> sig = new LinkedHashMap<String, Object>();
        sig.put("alg", "ECDSA-P256-SHA256");
        sig.put("key_id", dev.agentroid.helper.link.net.LinkCrypto.keyId(spki()));
        sig.put("value", Base64.getEncoder().encodeToString(s.sign()));
        Map<String, Object> env = new LinkedHashMap<String, Object>();
        env.put("segment", segment);
        env.put("signature", sig);
        return Json.write(env);
    }
}
