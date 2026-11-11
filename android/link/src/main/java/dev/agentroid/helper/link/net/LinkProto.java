package dev.agentroid.helper.link.net;

import java.nio.charset.StandardCharsets;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Set;
import java.util.regex.Pattern;

import dev.agentroid.helper.core.Json;

/**
 * Link protocol v1 (schemas/link.json is the source of truth; LinkProtoTest checks this class against it): message
 * types, repair reasons with their exact fix texts, frame builders, and a STRICT parser for frames from the hub
 * (unknown types and unknown fields are refused, numbers and strings are range- and pattern-checked).
 */
public final class LinkProto {
    private LinkProto() { }

    public static final int VERSION = 1;
    /** An envelope is the 64 KiB JSON-RPC request cap plus room for the envelope fields. */
    public static final int MAX_FRAME_BYTES = 64 * 1024 + 1024;

    // phone -> hub
    public static final String HELLO = "hello", CHALLENGE_RESPONSE = "challenge_response", HEARTBEAT = "heartbeat",
            RESPONSE = "response", REFUSED = "refused", UNTRUSTED_BSSID = "untrusted_bssid", HUMAN_TOUCH = "human_touch";
    // hub -> phone
    public static final String CHALLENGE = "challenge", READY = "ready", REQUEST = "request";
    /** A signed 2.14 runner segment (link.json $defs/segment), sequenced and answered exactly like a request. */
    public static final String SEGMENT = "segment";
    /** The method a segment frame is dispatched to (the hub's link_hub::proto::SEGMENT_METHOD). */
    public static final String SEGMENT_METHOD = "runner.segment";
    // both
    public static final String ACK = "ack", RESUME = "resume", NEEDS_REPAIR = "needs_repair";

    // repair reasons the phone detects
    public static final String R_HUB_KEY_MISSING = "hub_key_missing", R_BAD_HUB_SIGNATURE = "bad_hub_signature",
            R_SERIAL_MISMATCH = "serial_mismatch", R_STALE_EPOCH = "stale_epoch", R_BAD_CHALLENGE = "bad_challenge",
            R_KEY_UNAVAILABLE = "key_unavailable";
    // repair reasons the hub detects
    public static final String R_UNKNOWN_DEVICE_KEY = "unknown_device_key", R_BAD_PHONE_SIGNATURE = "bad_phone_signature",
            R_KEY_REVOKED = "key_revoked";

    // refusal reasons
    public static final String X_STALE_EPOCH = "stale_epoch", X_EPOCH_AHEAD = "epoch_ahead", X_NOT_READY = "not_ready",
            X_OUT_OF_ORDER = "out_of_order", X_BAD_REQUEST = "bad_request";

    private static final String PAIR = "On the laptop, with the phone reachable over adb, run: agentroid link pair <serial>";

    /** Exact fix text per reason, identical to the const values in link.json $defs/needs_repair. */
    public static final Map<String, String> PHONE_FIX;
    public static final Map<String, String> HUB_FIX;
    static {
        Map<String, String> p = new LinkedHashMap<String, String>();
        p.put(R_HUB_KEY_MISSING, "This phone has no hub key yet. " + PAIR);
        p.put(R_BAD_HUB_SIGNATURE, "The hub's signature did not match the hub key this phone holds (the laptop's hub key changed, "
                + "or another machine answered at the hub address). " + PAIR);
        p.put(R_SERIAL_MISMATCH, "The hub named a different device serial from the one this phone was paired as. " + PAIR);
        p.put(R_STALE_EPOCH, "A hub with an older epoch tried to take this phone. Stop the old hub process (only the newest hub "
                + "may own a phone); the current hub then reconnects by itself.");
        p.put(R_BAD_CHALLENGE, "The hub sent a challenge this phone could not read. Update the hub (bridged) and the helper app "
                + "so both speak link protocol v1, then let the hub reconnect.");
        p.put(R_KEY_UNAVAILABLE, "This phone's link key cannot sign (the Android Keystore refused it). " + PAIR + " --rekey");
        PHONE_FIX = Collections.unmodifiableMap(p);
        Map<String, String> h = new LinkedHashMap<String, String>();
        h.put(R_UNKNOWN_DEVICE_KEY, "The hub has no record of this phone's link key. " + PAIR);
        h.put(R_BAD_PHONE_SIGNATURE, "The phone's signature did not verify with the key the hub holds for it. " + PAIR);
        h.put(R_KEY_REVOKED, "This phone's link key was revoked on the hub. To let the phone back in, on the laptop, with the phone "
                + "reachable over adb, run: agentroid link pair <serial>");
        HUB_FIX = Collections.unmodifiableMap(h);
    }

    static final Pattern KEY_ID = Pattern.compile("[0-9a-f]{64}");
    static final Pattern SERIAL = Pattern.compile("[A-Za-z0-9._:-]{1,64}");
    static final Pattern REQUEST_ID = Pattern.compile("[A-Za-z0-9._:-]{1,128}");
    static final Pattern INSTANCE = Pattern.compile("[A-Za-z0-9_-]{16,64}");

    private static final Map<String, Set<String>> HUB_FIELDS = new LinkedHashMap<String, Set<String>>();
    static {
        HUB_FIELDS.put(CHALLENGE, set("v", "t", "epoch", "nonce", "phone_nonce", "serial", "hub_key_id", "hub_sig"));
        HUB_FIELDS.put(READY, set("v", "t", "epoch", "key_id"));
        HUB_FIELDS.put(REQUEST, set("v", "t", "epoch", "seq", "id", "rpc"));
        HUB_FIELDS.put(SEGMENT, set("v", "t", "epoch", "seq", "id", "segment"));
        HUB_FIELDS.put(ACK, set("v", "t", "epoch", "ack"));
        HUB_FIELDS.put(RESUME, set("v", "t", "epoch", "last_acked"));
        HUB_FIELDS.put(NEEDS_REPAIR, set("v", "t", "from", "reason", "fix", "epoch"));
    }

    private static Set<String> set(String... s) { return new HashSet<String>(Arrays.asList(s)); }

    /** A frame from the hub that this phone will not read. reason is short and safe to send back. */
    public static final class BadFrame extends Exception {
        public final String type;
        public final Object id;
        public BadFrame(String type, Object id, String reason) {
            super(reason);
            this.type = type;
            this.id = id;
        }
    }

    // ------------------------------------------------------------------ parse (hub -> phone)

    @SuppressWarnings("unchecked")
    public static Map<String, Object> parseFromHub(String text) throws BadFrame {
        if (text == null || text.isEmpty()) throw new BadFrame(null, null, "empty frame");
        if (text.getBytes(StandardCharsets.UTF_8).length > MAX_FRAME_BYTES) throw new BadFrame(null, null, "frame too large");
        Object root;
        try {
            root = Json.parse(text);
        } catch (Json.ParseException e) {
            throw new BadFrame(null, null, "invalid JSON");
        }
        if (!(root instanceof Map)) throw new BadFrame(null, null, "a frame is one JSON object");
        Map<String, Object> m = (Map<String, Object>) root;
        Object t = m.get("t");
        Object id = m.get("id") instanceof String ? m.get("id") : null;
        if (!(t instanceof String) || !HUB_FIELDS.containsKey(t)) throw new BadFrame(null, id, "unknown or missing type");
        String type = (String) t;
        if (!Long.valueOf(VERSION).equals(m.get("v"))) throw new BadFrame(type, id, "v must be 1");
        for (String k : m.keySet()) if (!HUB_FIELDS.get(type).contains(k)) throw new BadFrame(type, id, "unknown field " + k);
        if (type.equals(NEEDS_REPAIR)) {
            if (!"hub".equals(m.get("from"))) throw new BadFrame(type, id, "from must be hub");
            Object r = m.get("reason");
            if (!(r instanceof String) || !HUB_FIX.containsKey(r)) throw new BadFrame(type, id, "unknown repair reason");
            if (!(m.get("fix") instanceof String)) throw new BadFrame(type, id, "fix must be a string");
            if (m.containsKey("epoch")) nonNeg(m, "epoch", type, id);
            return m;
        }
        long epoch = num(m, "epoch", 1, type, id);
        if (type.equals(CHALLENGE)) {
            for (String k : new String[] {"nonce", "phone_nonce"}) {
                if (!(m.get(k) instanceof String) || !LinkCrypto.nonceOk((String) m.get(k))) throw new BadFrame(type, id, k + " is not 32..64 base64 bytes");
            }
            pattern(m, "serial", SERIAL, type, id);
            pattern(m, "hub_key_id", KEY_ID, type, id);
            Object sig = m.get("hub_sig");
            if (!(sig instanceof String) || LinkCrypto.unb64((String) sig) == null) throw new BadFrame(type, id, "hub_sig is not base64");
        } else if (type.equals(READY)) {
            pattern(m, "key_id", KEY_ID, type, id);
        } else if (type.equals(REQUEST)) {
            num(m, "seq", 1, type, id);
            pattern(m, "id", REQUEST_ID, type, id);
            Object rpc = m.get("rpc");
            if (!(rpc instanceof Map)) throw new BadFrame(type, id, "rpc must be an object");
            if (!m.get("id").equals(((Map<String, Object>) rpc).get("id"))) throw new BadFrame(type, id, "rpc.id must equal id");
        } else if (type.equals(SEGMENT)) {
            // the envelope only: the runner's SegmentVerifier checks the segment itself (signature, device, epoch...)
            num(m, "seq", 1, type, id);
            pattern(m, "id", REQUEST_ID, type, id);
            Object s = m.get("segment");
            if (!(s instanceof Map)) throw new BadFrame(type, id, "segment must be an object");
            Map<String, Object> env = (Map<String, Object>) s;
            if (env.size() != 2 || !(env.get("segment") instanceof Map) || !(env.get("signature") instanceof Map)) {
                throw new BadFrame(type, id, "segment must be {segment, signature}");
            }
        } else if (type.equals(ACK)) {
            nonNeg(m, "ack", type, id);
        } else if (type.equals(RESUME)) {
            nonNeg(m, "last_acked", type, id);
        }
        if (epoch < 1) throw new BadFrame(type, id, "epoch must be >= 1");
        return m;
    }

    /**
     * A parsed segment frame as the request it is answered like: same epoch, seq and id, and
     * rpc = {jsonrpc: "2.0", id, method: runner.segment, params: the signed segment}. The request path then applies
     * the epoch fence, seq == ack + 1, the outbox and the request-id cache unchanged.
     */
    public static Map<String, Object> segmentAsRequest(Map<String, Object> m) {
        Map<String, Object> rpc = new LinkedHashMap<String, Object>();
        rpc.put("jsonrpc", "2.0");
        rpc.put("id", m.get("id"));
        rpc.put("method", SEGMENT_METHOD);
        rpc.put("params", m.get("segment"));
        Map<String, Object> r = new LinkedHashMap<String, Object>(m);
        r.remove("segment");
        r.put("t", REQUEST);
        r.put("rpc", rpc);
        return r;
    }

    private static long num(Map<String, Object> m, String k, long min, String type, Object id) throws BadFrame {
        Object o = m.get(k);
        if (!(o instanceof Long) || (Long) o < min) throw new BadFrame(type, id, k + " must be an integer >= " + min);
        return (Long) o;
    }

    private static long nonNeg(Map<String, Object> m, String k, String type, Object id) throws BadFrame { return num(m, k, 0, type, id); }

    private static void pattern(Map<String, Object> m, String k, Pattern p, String type, Object id) throws BadFrame {
        Object o = m.get(k);
        if (!(o instanceof String) || !p.matcher((String) o).matches()) throw new BadFrame(type, id, k + " has the wrong form");
    }

    // ------------------------------------------------------------------ build (phone -> hub)

    public static Map<String, Object> frame(String t) {
        Map<String, Object> m = new LinkedHashMap<String, Object>();
        m.put("v", Long.valueOf(VERSION));
        m.put("t", t);
        return m;
    }

    public static Map<String, Object> needsRepair(String reason, long epoch) {
        Map<String, Object> m = frame(NEEDS_REPAIR);
        m.put("from", "phone");
        m.put("reason", reason);
        m.put("fix", PHONE_FIX.get(reason));
        m.put("epoch", Long.valueOf(epoch));
        return m;
    }

    public static Map<String, Object> refused(long epoch, String reason, Object id, Long reqSeq, Long reqEpoch) {
        Map<String, Object> m = frame(REFUSED);
        m.put("epoch", Long.valueOf(epoch));
        m.put("reason", reason);
        if (id instanceof String && REQUEST_ID.matcher((String) id).matches()) m.put("id", id);
        if (reqSeq != null && reqSeq >= 0) m.put("req_seq", reqSeq);
        if (reqEpoch != null && reqEpoch >= 0) m.put("req_epoch", reqEpoch);
        return m;
    }

    public static final Set<String> BSSID_REASONS = set("not_in_trusted_list", "no_trusted_list", "bssid_unreadable");
    static final Pattern BSSID = Pattern.compile("[0-9a-f]{2}(:[0-9a-f]{2}){5}");

    /** Body of an untrusted_bssid event, for LinkClient.sendEvent (which adds epoch, seq and ack). */
    public static Map<String, Object> untrustedBssid(String bssid, String reason, long tsMs) {
        if (!BSSID_REASONS.contains(reason)) throw new IllegalArgumentException("unknown reason");
        boolean unreadable = reason.equals("bssid_unreadable");
        if (!unreadable && (bssid == null || !BSSID.matcher(bssid).matches())) throw new IllegalArgumentException("bssid form");
        Map<String, Object> m = frame(UNTRUSTED_BSSID);
        m.put("bssid", unreadable ? null : bssid);
        m.put("reason", reason);
        m.put("action", "wifi_adb_not_reasserted");
        m.put("ts_ms", Long.valueOf(tsMs));
        return m;
    }

    /**
     * Body of a human_touch event (the overlay's Detector 2), for LinkClient.sendEvent. Both times are
     * SystemClock.elapsedRealtime(): when the touch happened and when this frame is made. The hub dates it from them.
     */
    public static Map<String, Object> humanTouch(long touchMs, long sentMs) {
        if (touchMs < 0 || sentMs < touchMs) throw new IllegalArgumentException("need 0 <= touch_ms <= sent_ms");
        Map<String, Object> m = frame(HUMAN_TOUCH);
        m.put("touch_ms", Long.valueOf(touchMs));
        m.put("sent_ms", Long.valueOf(sentMs));
        return m;
    }

    public static String write(Map<String, Object> m) { return Json.write(m); }
}
