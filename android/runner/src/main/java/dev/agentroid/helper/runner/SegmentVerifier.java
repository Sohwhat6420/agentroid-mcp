package dev.agentroid.helper.runner;

import dev.agentroid.helper.core.Clock;
import dev.agentroid.helper.core.Json;
import dev.agentroid.helper.link.net.LinkCrypto;
import dev.agentroid.helper.link.net.TrustStore;

import java.util.Arrays;
import java.util.List;
import java.util.Map;

/**
 * Checks a segment before anything runs: the hub signature with the key pinned by link pairing, then the epoch, the
 * device, the expiry and the structure. Uses the link's TrustStore READ ONLY (hubKeySpki, serial, maxEpoch) and
 * LinkCrypto for the P-256 SHA256withECDSA check, so there is one crypto call site in the app.
 *
 * Order matters: the signature is checked before any field is believed. The classification of each step is checked
 * separately by SegmentRunner against the bundled safe list.
 */
public final class SegmentVerifier {
    public static final String ALG = "ECDSA-P256-SHA256";
    private static final List<String> ENVELOPE = Arrays.asList("segment", "signature");
    private static final List<String> SIG_KEYS = Arrays.asList("alg", "key_id", "value");

    private final TrustStore trust;
    private final Clock clock;

    public SegmentVerifier(TrustStore trust, Clock clock) {
        this.trust = trust;
        this.clock = clock;
    }

    /** The epoch of the hub this phone last reached ready with (persisted by the link). */
    public long currentEpoch() { return trust.maxEpoch(); }

    /** ro.serialno as paired, or null. */
    public String phoneSerial() { return trust.serial(); }

    @SuppressWarnings("unchecked")
    public Segment verify(String json) throws SegmentRefused {
        Object top;
        try {
            top = Json.parse(json);
        } catch (Json.ParseException e) {
            throw new SegmentRefused("malformed", e.getMessage(), null);
        }
        if (!(top instanceof Map)) throw new SegmentRefused("malformed", "not an object", null);
        Map<String, Object> env = (Map<String, Object>) top;
        String id = Segment.idOf(env.get("segment"));
        for (String k : env.keySet()) if (!ENVELOPE.contains(k)) throw new SegmentRefused("malformed", k + " is not allowed", id);
        if (!(env.get("segment") instanceof Map)) throw new SegmentRefused("malformed", "segment must be an object", id);
        if (!(env.get("signature") instanceof Map)) throw new SegmentRefused("unsigned", "no signature", id);
        Map<String, Object> sig = (Map<String, Object>) env.get("signature");
        for (String k : sig.keySet()) if (!SIG_KEYS.contains(k)) throw new SegmentRefused("malformed", "signature." + k + " is not allowed", id);
        Object value = sig.get("value");
        if (!(value instanceof String) || ((String) value).isEmpty()) throw new SegmentRefused("unsigned", "empty signature", id);
        if (!ALG.equals(sig.get("alg"))) throw new SegmentRefused("bad_alg", String.valueOf(sig.get("alg")), id);

        byte[] spki = trust.hubKeySpki();
        if (spki == null) throw new SegmentRefused("hub_key_missing", "the phone was never paired", id);
        if (!LinkCrypto.keyId(spki).equals(sig.get("key_id"))) throw new SegmentRefused("wrong_hub_key", "key_id is not the pinned hub key", id);
        byte[] der = LinkCrypto.unb64((String) value);
        if (der == null) throw new SegmentRefused("bad_signature", "signature is not canonical base64", id);
        Map<String, Object> segMap = (Map<String, Object>) env.get("segment");
        byte[] msg;
        try {
            msg = Canonical.signedBytes(segMap);
        } catch (IllegalArgumentException e) {
            throw new SegmentRefused("invalid", e.getMessage(), id);
        }
        if (!LinkCrypto.verify(spki, msg, der)) throw new SegmentRefused("bad_signature", "signature does not verify", id);

        Segment seg;
        try {
            seg = Segment.parse(segMap);
        } catch (Segment.Malformed e) {
            throw new SegmentRefused("invalid", e.getMessage(), id);
        }
        long current = trust.maxEpoch();
        if (seg.epoch < current) throw new SegmentRefused("stale_epoch", "segment epoch " + seg.epoch + " < hub epoch " + current, id);
        if (seg.epoch > current) throw new SegmentRefused("epoch_ahead", "segment epoch " + seg.epoch + " > hub epoch " + current, id);
        String serial = trust.serial();
        if (serial == null || !serial.equals(seg.device)) throw new SegmentRefused("wrong_device", "segment is for " + seg.device, id);
        long now = clock.wallMs();
        if (seg.expiresMs <= now) throw new SegmentRefused("expired", "expired at " + seg.expires, id);
        List<String> errs = seg.check(now);
        if (!errs.isEmpty()) throw new SegmentRefused("invalid", errs.toString(), id);
        if (seg.classification.ordinal() > Risk.R1.ordinal()) throw new SegmentRefused("classification_not_r0_r1", seg.classification.name(), id);
        return seg;
    }
}
