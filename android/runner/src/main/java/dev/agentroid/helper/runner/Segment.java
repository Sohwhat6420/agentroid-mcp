package dev.agentroid.helper.runner;

import java.time.Instant;
import java.time.format.DateTimeParseException;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.regex.Pattern;

/**
 * A parsed segment (schemas/segment.json, the "segment" object). Parsing is strict, like the schema: unknown keys,
 * nulls and wrong types are refused. Rust twin: crates/segments/src/segment.rs (Segment, Step, Post,
 * structural_errors). The raw map is kept, because the signature is checked over the canonical form of exactly what
 * arrived.
 */
public final class Segment {
    public static final int MAX_STEPS = 500;
    public static final long MAX_TTL_S = 3600;
    public static final long CLOCK_SKEW_S = 300;
    private static final Pattern UTC = Pattern.compile("^\\d{4}-\\d{2}-\\d{2}T\\d{2}:\\d{2}:\\d{2}Z$");
    private static final List<String> SEGMENT_KEYS = Arrays.asList("v", "segment_id", "run_id", "device", "epoch", "owned", "classification", "expires", "steps");
    private static final List<String> STEP_REQUIRED = Arrays.asList("id", "kind", "package", "post", "risk");
    private static final List<String> STEP_OPTIONAL = Arrays.asList("select", "value", "timeout_ms", "direction", "namespace", "key", "scope", "lease");
    static final List<String> POST_KINDS = Arrays.asList("none", "present", "absent", "text_equals", "setting_equals");
    private static final List<String> SELECT_KEYS = Arrays.asList("id", "text", "desc", "near_text");

    public static final class Malformed extends Exception {
        public Malformed(String m) { super(m); }
    }

    public static final class Post {
        public final String kind;
        /** One-entry map, or null. */
        public final Map<String, Object> select;
        public final String text;

        Post(String kind, Map<String, Object> select, String text) { this.kind = kind; this.select = select; this.text = text; }

        public boolean readsScreen() { return kind.equals("present") || kind.equals("absent") || kind.equals("text_equals"); }
    }

    public static final class Step {
        public final String id, kind, pkg;
        public final Map<String, Object> select;
        public final Post post;
        /** The hub's label. Never trusted alone: SafeClasses re-derives the class. */
        public final Risk label;
        /** Long or String, or null. */
        public final Object value;
        public final Long timeoutMs;
        public final String direction, namespace, key, scope, lease;

        Step(String id, String kind, String pkg, Map<String, Object> select, Post post, Risk label, Object value, Long timeoutMs,
             String direction, String namespace, String key, String scope, String lease) {
            this.id = id; this.kind = kind; this.pkg = pkg; this.select = select; this.post = post; this.label = label;
            this.value = value; this.timeoutMs = timeoutMs; this.direction = direction; this.namespace = namespace;
            this.key = key; this.scope = scope; this.lease = lease;
        }
    }

    public final int v;
    public final String segmentId, runId, device;
    public final long epoch;
    public final boolean owned;
    public final Risk classification;
    public final String expires;
    /** expires as Unix ms. */
    public final long expiresMs;
    public final List<Step> steps;
    public final Map<String, Object> raw;

    private Segment(Map<String, Object> raw, int v, String segmentId, String runId, String device, long epoch, boolean owned,
                    Risk classification, String expires, long expiresMs, List<Step> steps) {
        this.raw = raw; this.v = v; this.segmentId = segmentId; this.runId = runId; this.device = device; this.epoch = epoch;
        this.owned = owned; this.classification = classification; this.expires = expires; this.expiresMs = expiresMs;
        this.steps = Collections.unmodifiableList(steps);
    }

    /** The segment id if the object has a string one, for refusal reports; else null. */
    public static String idOf(Object segment) {
        if (!(segment instanceof Map)) return null;
        Object id = ((Map<?, ?>) segment).get("segment_id");
        return id instanceof String ? (String) id : null;
    }

    /** Unix ms for YYYY-MM-DDTHH:MM:SSZ, or null. */
    public static Long parseUtcMs(String s) {
        if (s == null || !UTC.matcher(s).matches()) return null;
        try {
            return Instant.parse(s).toEpochMilli();
        } catch (DateTimeParseException e) {
            return null;
        }
    }

    @SuppressWarnings("unchecked")
    public static Segment parse(Object o) throws Malformed {
        Map<String, Object> m = map(o, "segment");
        exactKeys(m, SEGMENT_KEYS, Collections.<String>emptyList(), "segment");
        long v = integer(m.get("v"), "v");
        String segmentId = string(m.get("segment_id"), "segment_id");
        String runId = string(m.get("run_id"), "run_id");
        String device = string(m.get("device"), "device");
        long epoch = integer(m.get("epoch"), "epoch");
        if (epoch < 0) throw new Malformed("epoch is negative");
        if (!(m.get("owned") instanceof Boolean)) throw new Malformed("owned must be a boolean");
        Risk cls = Risk.parse(m.get("classification"));
        if (cls == null) throw new Malformed("classification must be R0..R5");
        String expires = string(m.get("expires"), "expires");
        Long expiresMs = parseUtcMs(expires);
        if (expiresMs == null) throw new Malformed("expires must be YYYY-MM-DDTHH:MM:SSZ");
        if (!(m.get("steps") instanceof List)) throw new Malformed("steps must be an array");
        List<Step> steps = new ArrayList<Step>();
        int i = 0;
        for (Object so : (List<Object>) m.get("steps")) steps.add(step(so, "steps/" + (i++)));
        return new Segment(m, (int) v, segmentId, runId, device, epoch, ((Boolean) m.get("owned")).booleanValue(), cls, expires, expiresMs.longValue(), steps);
    }

    private static Step step(Object o, String p) throws Malformed {
        Map<String, Object> m = map(o, p);
        exactKeys(m, STEP_REQUIRED, STEP_OPTIONAL, p);
        Risk label = Risk.parse(m.get("risk"));
        if (label == null) throw new Malformed(p + ": risk must be R0..R5");
        Object value = null;
        if (m.containsKey("value")) {
            value = m.get("value");
            if (!(value instanceof String) && !(value instanceof Long)) throw new Malformed(p + ": value must be an integer or a string");
        }
        Long timeout = null;
        if (m.containsKey("timeout_ms")) {
            timeout = integer(m.get("timeout_ms"), p + "/timeout_ms");
            if (timeout.longValue() < 0) throw new Malformed(p + ": timeout_ms is negative");
        }
        return new Step(string(m.get("id"), p + "/id"), string(m.get("kind"), p + "/kind"), string(m.get("package"), p + "/package"),
                m.containsKey("select") ? select(m.get("select"), p + "/select") : null, post(m.get("post"), p + "/post"), label, value, timeout,
                opt(m, "direction", p), opt(m, "namespace", p), opt(m, "key", p), opt(m, "scope", p), opt(m, "lease", p));
    }

    private static Post post(Object o, String p) throws Malformed {
        Map<String, Object> m = map(o, p);
        exactKeys(m, Collections.singletonList("kind"), Arrays.asList("select", "text"), p);
        return new Post(string(m.get("kind"), p + "/kind"), m.containsKey("select") ? select(m.get("select"), p + "/select") : null,
                m.containsKey("text") ? strAllowEmpty(m.get("text"), p + "/text") : null);
    }

    private static Map<String, Object> select(Object o, String p) throws Malformed {
        Map<String, Object> m = map(o, p);
        if (m.size() != 1) throw new Malformed(p + ": exactly one selector key");
        Map.Entry<String, Object> e = m.entrySet().iterator().next();
        if (!SELECT_KEYS.contains(e.getKey())) throw new Malformed(p + ": " + e.getKey() + " is not one of " + SELECT_KEYS);
        string(e.getValue(), p + "/" + e.getKey());
        return Collections.unmodifiableMap(new LinkedHashMap<String, Object>(m));
    }

    /**
     * Structural rules shared with the hub (Rust structural_errors). nowMs bounds the expiry: a segment may not live
     * longer than MAX_TTL_S (+ skew), so a hub with a wrong clock cannot mint one that never expires.
     */
    public List<String> check(long nowMs) {
        List<String> e = new ArrayList<String>();
        if (v != 1) e.add("v must be 1");
        if (steps.isEmpty()) e.add("a segment needs at least one step");
        if (steps.size() > MAX_STEPS) e.add("more than " + MAX_STEPS + " steps");
        if (expiresMs > nowMs + (MAX_TTL_S + CLOCK_SKEW_S) * 1000) e.add("expires is more than " + MAX_TTL_S + " s ahead");
        Set<String> ids = new HashSet<String>();
        for (int i = 0; i < steps.size(); i++) {
            Step s = steps.get(i);
            String p = "steps/" + i;
            if (!ids.add(s.id)) e.add(p + ": id " + s.id + " repeats");
            boolean needsSelect = s.kind.equals("tap") || s.kind.equals("read") || s.kind.equals("assert");
            if (needsSelect && s.select == null) e.add(p + ": " + s.kind + " needs select");
            if (s.kind.equals("settings_set") && (s.key == null || s.namespace == null || s.value == null || s.scope == null)) {
                e.add(p + ": settings_set needs namespace, key, value and scope");
            }
            if (!POST_KINDS.contains(s.post.kind)) e.add(p + "/post: kind " + s.post.kind + " is unknown");
            if (s.post.readsScreen() && s.post.select == null) e.add(p + "/post: " + s.post.kind + " needs a select");
            if (!s.post.readsScreen() && (s.post.select != null || s.post.text != null)) e.add(p + "/post: " + s.post.kind + " takes no select or text");
            if (s.post.kind.equals("text_equals") && s.post.text == null) e.add(p + "/post: text_equals needs text");
            if (s.post.kind.equals("setting_equals") && !s.kind.equals("settings_set")) e.add(p + "/post: setting_equals is for settings_set only");
            if ((s.kind.equals("tap") || s.kind.equals("settings_set")) && s.post.kind.equals("none")) e.add(p + ": " + s.kind + " needs a post-condition");
        }
        return e;
    }

    // ------------------------------------------------------------------ strict readers

    @SuppressWarnings("unchecked")
    private static Map<String, Object> map(Object o, String p) throws Malformed {
        if (!(o instanceof Map)) throw new Malformed(p + " must be an object");
        return (Map<String, Object>) o;
    }

    private static void exactKeys(Map<String, Object> m, List<String> required, List<String> optional, String p) throws Malformed {
        for (String k : required) if (!m.containsKey(k)) throw new Malformed(p + ": " + k + " is required");
        for (String k : m.keySet()) {
            if (!required.contains(k) && !optional.contains(k)) throw new Malformed(p + ": " + k + " is not allowed");
            if (m.get(k) == null) throw new Malformed(p + ": " + k + " is null");
        }
    }

    private static String string(Object o, String p) throws Malformed {
        if (!(o instanceof String) || ((String) o).isEmpty()) throw new Malformed(p + " must be a non-empty string");
        return (String) o;
    }

    private static String strAllowEmpty(Object o, String p) throws Malformed {
        if (!(o instanceof String)) throw new Malformed(p + " must be a string");
        return (String) o;
    }

    private static long integer(Object o, String p) throws Malformed {
        if (!(o instanceof Long)) throw new Malformed(p + " must be an integer");
        return ((Long) o).longValue();
    }

    private static String opt(Map<String, Object> m, String k, String p) throws Malformed {
        return m.containsKey(k) ? string(m.get(k), p + "/" + k) : null;
    }
}
