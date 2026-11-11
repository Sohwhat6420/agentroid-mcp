package dev.agentroid.helper.runner;

import dev.agentroid.helper.core.Json;

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

/**
 * The hand-written safe list (runner/safe-classes.json, round 38), bundled as the Java resource /safe-classes.json, and
 * the per-step decision. Rust twin: crates/segments/src/classes.rs; the module docs there are the spec, and both sides
 * read the same file.
 *
 * The phone never takes the hub's word for a step's class: it derives the safe kind from the step itself, looks it up
 * here, and the hub's label can only RAISE the class (effective = max(listed, label)). R0/R1 is allowed, R2 is
 * require_confirmation (blocked in Phase 2), R3+ is blocked.
 */
public final class SafeClasses {
    public static final String PLAY_STORE = "com.android.vending";
    public static final String RESOURCE = "/safe-classes.json";

    /** One decision, in the shape of Gemini Computer Use's safety_decision ({decision, explanation}). */
    public static final class StepDecision {
        public final String stepId, safeKind, pkg, explanation;
        public final Risk listed, effective;
        public final Decision decision, finalDecision;

        StepDecision(String stepId, String safeKind, String pkg, Risk listed, Risk effective, Decision decision, Decision finalDecision, String explanation) {
            this.stepId = stepId; this.safeKind = safeKind; this.pkg = pkg; this.listed = listed; this.effective = effective;
            this.decision = decision; this.finalDecision = finalDecision; this.explanation = explanation;
        }
    }

    private static final class ClassList {
        final List<String> kinds, packages;
        ClassList(List<String> kinds, List<String> packages) { this.kinds = kinds; this.packages = packages; }
    }

    private final ClassList r0, r1;

    private SafeClasses(ClassList r0, ClassList r1) { this.r0 = r0; this.r1 = r1; }

    /** The list bundled with the app. */
    public static SafeClasses bundled() throws IOException {
        InputStream in = SafeClasses.class.getResourceAsStream(RESOURCE);
        if (in == null) throw new IOException(RESOURCE + " is not on the classpath");
        try {
            ByteArrayOutputStream b = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int n;
            while ((n = in.read(buf)) > 0) b.write(buf, 0, n);
            return parse(new String(b.toByteArray(), StandardCharsets.UTF_8));
        } finally {
            in.close();
        }
    }

    /** Every rule of schemas/safe-classes.json (as the Rust parser): only "classes"; only R0/R1; exactly step_kinds and packages; unique strings. */
    @SuppressWarnings("unchecked")
    public static SafeClasses parse(String text) throws IOException {
        Object top;
        try {
            top = Json.parse(text);
        } catch (Json.ParseException e) {
            throw new IOException("safe-classes is not JSON: " + e.getMessage());
        }
        if (!(top instanceof Map) || ((Map<String, Object>) top).size() != 1 || !(((Map<String, Object>) top).get("classes") instanceof Map)) {
            throw new IOException("safe-classes must be {\"classes\": {...}}");
        }
        Map<String, Object> cl = (Map<String, Object>) ((Map<String, Object>) top).get("classes");
        if (cl.isEmpty()) throw new IOException("classes needs at least one class");
        ClassList r0 = null, r1 = null;
        for (Map.Entry<String, Object> e : cl.entrySet()) {
            if (!(e.getValue() instanceof Map)) throw new IOException(e.getKey() + " must be an object");
            Map<String, Object> body = (Map<String, Object>) e.getValue();
            if (body.size() != 2 || !body.containsKey("step_kinds") || !body.containsKey("packages")) {
                throw new IOException(e.getKey() + " must have exactly step_kinds and packages");
            }
            ClassList l = new ClassList(strings(body.get("step_kinds"), e.getKey()), strings(body.get("packages"), e.getKey()));
            if (e.getKey().equals("R0")) r0 = l;
            else if (e.getKey().equals("R1")) r1 = l;
            else throw new IOException("class " + e.getKey() + " is not allowed: R0 and R1 only");
        }
        return new SafeClasses(r0, r1);
    }

    private static List<String> strings(Object o, String cls) throws IOException {
        if (!(o instanceof List)) throw new IOException(cls + ": arrays expected");
        List<String> out = new ArrayList<String>();
        for (Object x : (List<?>) o) {
            if (!(x instanceof String)) throw new IOException(cls + ": strings expected");
            if (out.contains(x)) throw new IOException(cls + " repeats " + x);
            out.add((String) x);
        }
        return out;
    }

    private static boolean allows(ClassList l, String safeKind, String pkg) {
        if (l == null) return false;
        if (l.kinds.contains(safeKind + "@" + pkg)) return true;
        return l.kinds.contains(safeKind) && (l.packages.contains("*") || l.packages.contains(pkg));
    }

    /** The class the list gives (safe kind, package): R0, R1, or R2 when it is not on the list. */
    public Risk listedClass(String safeKind, String pkg) {
        if (allows(r0, safeKind, pkg)) return Risk.R0;
        if (allows(r1, safeKind, pkg)) return Risk.R1;
        return Risk.R2;
    }

    /** Rust twin: classes::derive_safe_kind. owned = the segment says owned AND this phone knows it is owned. */
    public static String deriveSafeKind(Segment.Step s, boolean owned) {
        String k = s.kind;
        if (k.equals("read") || k.equals("assert") || k.equals("wait") || k.equals("scroll") || k.equals("back")) return k;
        if (k.equals("tap")) {
            if (s.select == null || s.select.size() != 1) return "tap";
            Map.Entry<String, Object> e = s.select.entrySet().iterator().next();
            Object val = e.getValue();
            if (s.pkg.equals(PLAY_STORE) && owned && e.getKey().equals("text") && "Update".equals(val)) return "play_store_update";
            boolean unique = e.getKey().equals("id") || e.getKey().equals("text") || e.getKey().equals("desc");
            return unique && val instanceof String && !((String) val).isEmpty() ? "tap_unique" : "tap";
        }
        if (k.equals("settings_set")) {
            boolean leased = s.lease != null && !s.lease.isEmpty();
            boolean reversible = "job".equals(s.scope) || "session".equals(s.scope);
            boolean toggle = "0".equals(s.value) || "1".equals(s.value);
            boolean ns = "system".equals(s.namespace) || "secure".equals(s.namespace) || "global".equals(s.namespace);
            return leased && reversible && toggle && ns ? "settings_toggle_leased" : "settings_set";
        }
        return k;
    }

    /** The decision for one step, with the Phase 2 rule applied as finalDecision. */
    public StepDecision decide(Segment.Step s, boolean owned) {
        String safeKind = deriveSafeKind(s, owned);
        Risk listed = listedClass(safeKind, s.pkg);
        Risk effective = Risk.max(listed, s.label);
        Decision d = effective.ordinal() <= Risk.R1.ordinal() ? Decision.ALLOWED
                : effective == Risk.R2 ? Decision.REQUIRE_CONFIRMATION : Decision.BLOCKED;
        Decision fin = Decision.phaseTwo(d);
        String why;
        if (d == Decision.ALLOWED) why = safeKind + " in " + s.pkg + " is " + effective + " on the safe list";
        else if (d == Decision.REQUIRE_CONFIRMATION && listed == Risk.R2) why = safeKind + " in " + s.pkg + " is not on the safe list (R2): needs the device owner's approval, and Phase 2 has no approval channel";
        else if (d == Decision.REQUIRE_CONFIRMATION) why = "the hub labelled it " + s.label + ": needs the device owner's approval, and Phase 2 has no approval channel";
        else why = effective + " is never run by the on-phone runner";
        return new StepDecision(s.id, safeKind, s.pkg, listed, effective, d, fin, why);
    }
}
