package dev.agentroid.helper.runner;

import dev.agentroid.helper.core.Clock;
import dev.agentroid.helper.core.Json;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * The on-phone step runner (task 2.14). Runs one signed R0/R1 segment at a time, locally, with post-conditions, and
 * queues a report for the hub. Single-threaded on a core Clock (the app uses the link's ExecutorClock thread; the
 * tests use a virtual clock), like LinkClient: every entry point must be called on that thread.
 *
 * Before the first step: SegmentVerifier (signature with the pinned hub key, epoch, device, expiry, structure), then
 * EVERY step is re-classified against the bundled safe list; any step whose decision is not "allowed" refuses the whole
 * segment and each such step is journalled. The hub's labels are never enough on their own.
 *
 * While running, before each step and on every poll: a human touch (the 2.15 flag) stops the segment and no further
 * step starts (an in-flight step is reported "interrupted", post-condition unverified, for the hub to check on resume);
 * an expired segment stops ("expired"); a newer hub epoch stops it ("stale_epoch"). Polls run every POLL_MS during a
 * post-condition and at most every TOUCH_POLL_MS during a wait, so the touch decision lands within 100 ms even without
 * the flag's push listener (well inside 2.14's 300 ms).
 *
 * The link is not needed to run: the report waits in the ReportOutbox until the link is up (laptop sleep).
 */
public final class SegmentRunner {
    public static final long POLL_MS = 50;
    public static final long TOUCH_POLL_MS = 100;
    public static final long DEFAULT_TIMEOUT_MS = 5000;

    /** Called on the Clock thread when a segment ends (any outcome), with the report that was queued (null if none). */
    public interface Listener {
        void onFinished(String outcome, String reportJson);
    }

    private final Clock clock;
    private final SegmentVerifier verifier;
    private final SafeClasses classes;
    private final StepExecutor exec;
    private final HumanTouch touch;
    private final RunnerJournal journal;
    private final ReportOutbox outbox;
    private final boolean ownedPhone;
    private final Listener listener;

    private Segment seg;
    private boolean running;
    private int gen;
    private int index;
    private long startMono;
    private long stepStartMono;
    private boolean inFlight;
    private long deadline;
    private long waitUntil;
    private String readValue;
    private List<Map<String, Object>> entries;
    private List<SafeClasses.StepDecision> decisions;

    /**
     * @param ownedPhone this phone knows it is owned (the full flavour); the Play Store Update token needs both this
     *                   and the segment's signed owned flag.
     */
    public SegmentRunner(Clock clock, SegmentVerifier verifier, SafeClasses classes, StepExecutor exec, HumanTouch touch,
                         RunnerJournal journal, ReportOutbox outbox, boolean ownedPhone, Listener listener) {
        this.clock = clock; this.verifier = verifier; this.classes = classes; this.exec = exec; this.touch = touch;
        this.journal = journal; this.outbox = outbox; this.ownedPhone = ownedPhone; this.listener = listener;
    }

    /** How many started segment ids are remembered, and for how long (a segment id runs once per phone). */
    public static final int ADMITTED_KEPT = 256;
    public static final long ADMITTED_MS = 24L * 3600_000L;

    /** Segment ids this runner started, with the monotonic time they started: a repeat is refused, never run twice. */
    private final LinkedHashMap<String, Long> admitted = new LinkedHashMap<String, Long>();

    public boolean busy() { return running; }

    /** The id an envelope claims, read before verification; used only to refuse a repeat, never to trust anything. */
    private static String claimedId(String json) {
        try {
            Object top = Json.parse(json);
            return top instanceof Map ? Segment.idOf(((Map<?, ?>) top).get("segment")) : null;
        } catch (Json.ParseException e) {
            return null;
        }
    }

    private boolean wasAdmitted(String id) {
        long now = clock.nowMs();
        java.util.Iterator<Long> it = admitted.values().iterator();
        while (it.hasNext()) {
            if (now - it.next().longValue() > ADMITTED_MS) it.remove();
            else break; // oldest first: the rest are younger
        }
        return admitted.containsKey(id);
    }

    private void remember(String id) {
        admitted.remove(id);
        admitted.put(id, Long.valueOf(clock.nowMs()));
        while (admitted.size() > ADMITTED_KEPT) {
            java.util.Iterator<String> it = admitted.keySet().iterator();
            it.next();
            it.remove();
        }
    }

    /**
     * Verify, re-classify and start a segment (on the Clock thread). Refusals are journalled and reported. The
     * returned Admission tells the caller at once whether the segment was taken (it then runs on the clock and its
     * report follows) or refused, and why; this is what the link's runner.segment answer carries.
     */
    public Admission submit(String json) {
        // A segment id runs once. A repeat of one that started (it may still be running, or finished while the hub
        // never heard the answer, as after a runner timeout) is a definite refusal, and an ordinary one: the link
        // remembers it. No report is queued for it, so the first run's report is never overwritten.
        String claimed = claimedId(json);
        if (claimed != null && wasAdmitted(claimed)) {
            journal(claimed, null, "refused", null, "duplicate_segment: this segment id was already started");
            return Admission.refused("refused", "duplicate_segment", "this phone already started segment " + claimed + ": it is not run twice", claimed);
        }
        if (running) {
            journal(null, null, "refused", null, "busy: a segment is already running");
            if (listener != null) listener.onFinished("busy", null);
            return Admission.refused("busy", "busy", "a segment is already running", null);
        }
        Segment s;
        try {
            s = verifier.verify(json);
        } catch (SegmentRefused e) {
            journal(e.segmentId, null, "refused", null, e.getMessage());
            String report = null;
            if (e.segmentId != null) {
                report = reportJson(e.segmentId, "", verifier.phoneSerial(), verifier.currentEpoch(), "refused", new ArrayList<Map<String, Object>>(), e.code);
                queue(e.segmentId, report);
            }
            if (listener != null) listener.onFinished("refused", report);
            return Admission.refused("refused", e.code, e.getMessage(), e.segmentId);
        }
        seg = s;
        entries = new ArrayList<Map<String, Object>>();
        decisions = new ArrayList<SafeClasses.StepDecision>();
        // The phone's own classification of every step, before anything runs.
        boolean safe = true;
        Risk actual = Risk.R0;
        for (Segment.Step st : s.steps) {
            SafeClasses.StepDecision d = classes.decide(st, s.owned && ownedPhone);
            decisions.add(d);
            actual = Risk.max(actual, d.effective);
            if (d.finalDecision != Decision.ALLOWED) {
                safe = false;
                journal(s.segmentId, st.id, "refused", d.finalDecision.wire, d.explanation);
                entries.add(entry(st.id, "refused", false, d.finalDecision.wire, null, d.explanation, null));
            }
        }
        if (!safe) {
            end("refused", "step_not_safe");
            return Admission.refused("refused", "step_not_safe", "a step is not allowed by the phone's own safe list", s.segmentId);
        }
        if (actual.ordinal() > s.classification.ordinal()) {
            journal(s.segmentId, null, "refused", null, "classification_too_low: claimed " + s.classification + ", steps need " + actual);
            end("refused", "classification_too_low");
            return Admission.refused("refused", "classification_too_low", "claimed " + s.classification + ", steps need " + actual, s.segmentId);
        }
        remember(s.segmentId);
        running = true;
        gen++;
        index = 0;
        startMono = clock.nowMs();
        final int myGen = gen;
        touch.setListener(new Runnable() {
            public void run() {
                clock.schedule(0, new Runnable() {
                    public void run() { if (myGen == gen && running && touched()) stopHuman(inFlight ? current() : null); }
                });
            }
        });
        journal(s.segmentId, null, "started", null, s.steps.size() + " steps, " + s.classification);
        runStep();
        return Admission.started(s.segmentId);
    }

    // ------------------------------------------------------------------ the step loop

    private Segment.Step current() { return seg.steps.get(index); }

    private boolean touched() {
        long t = touch.lastTouchMs();
        return t != HumanTouch.NEVER && t >= startMono;
    }

    private void later(long delay, final Runnable r) {
        final int myGen = gen;
        clock.schedule(delay, new Runnable() {
            public void run() { if (myGen == gen && running) r.run(); }
        });
    }

    /**
     * The three reasons to stop, checked before every step and on every poll: a human touch, expiry, a newer hub
     * epoch. A step in flight is reported interrupted (post-condition unverified). True when the segment stopped.
     */
    private boolean stopped(Segment.Step inFlightStep) {
        if (touched()) { stopHuman(inFlightStep); return true; }
        if (clock.wallMs() >= seg.expiresMs) { interrupt(inFlightStep, "expired"); end("expired", "expired at " + seg.expires); return true; }
        if (verifier.currentEpoch() != seg.epoch) { interrupt(inFlightStep, "stale_epoch"); end("stale_epoch", "hub epoch is now " + verifier.currentEpoch()); return true; }
        return false;
    }

    private void interrupt(Segment.Step step, String why) {
        if (step == null) return;
        entries.add(entry(step.id, "interrupted", false, decisions.get(index).finalDecision.wire, clock.nowMs() - stepStartMono, why, null));
        journal(seg.segmentId, step.id, "interrupted", null, why);
    }

    private void runStep() {
        if (stopped(null)) return;
        if (index >= seg.steps.size()) { end("completed", null); return; }
        final Segment.Step s = current();
        stepStartMono = clock.nowMs();
        inFlight = true;
        readValue = null;
        journal(seg.segmentId, s.id, "started", decisions.get(index).finalDecision.wire, null);
        if (s.kind.equals("wait")) {
            waitUntil = stepStartMono + (s.value instanceof Long ? ((Long) s.value).longValue() : 0L);
            tickWait();
            return;
        }
        if (s.kind.equals("read") || s.kind.equals("assert")) {
            int n = exec.count(s.select);
            if (s.kind.equals("assert") && n < 1) { stepFailed(s, "assert: nothing matches " + s.select); return; }
            if (s.kind.equals("read")) {
                if (n != 1) { stepFailed(s, "read needs exactly one match, found " + n); return; }
                readValue = exec.read(s.select);
            }
            afterAct(s);
            return;
        }
        if (s.kind.equals("tap")) {
            int n = exec.count(s.select);
            if (n != 1) { stepFailed(s, "tap needs exactly one match (unique selector), found " + n); return; }
        }
        String err = exec.act(s);
        if (err != null) { stepFailed(s, err); return; }
        afterAct(s);
    }

    private void tickWait() {
        Segment.Step s = current();
        if (stopped(s)) return;
        long remaining = waitUntil - clock.nowMs();
        if (remaining <= 0) { afterAct(s); return; }
        later(Math.min(remaining, TOUCH_POLL_MS), new Runnable() { public void run() { tickWait(); } });
    }

    private void afterAct(Segment.Step s) {
        deadline = clock.nowMs() + (s.timeoutMs != null ? s.timeoutMs.longValue() : DEFAULT_TIMEOUT_MS);
        pollPost();
    }

    private void pollPost() {
        Segment.Step s = current();
        if (stopped(s)) return;
        if (postHolds(s)) { stepDone(s); return; }
        if (clock.nowMs() >= deadline) { stepFailed(s, "post-condition " + s.post.kind + " did not hold in time"); return; }
        later(POLL_MS, new Runnable() { public void run() { pollPost(); } });
    }

    /** "none" holds at once (reads, waits, scrolls and back may carry none; taps and settings may not). */
    private boolean postHolds(Segment.Step s) {
        Segment.Post p = s.post;
        if (p.kind.equals("none")) return true;
        if (p.kind.equals("present")) return exec.count(p.select) >= 1;
        if (p.kind.equals("absent")) return exec.count(p.select) == 0;
        if (p.kind.equals("text_equals")) return exec.count(p.select) == 1 && p.text.equals(exec.read(p.select));
        if (p.kind.equals("setting_equals")) return String.valueOf(s.value).equals(exec.setting(s.namespace, s.key));
        return false;
    }

    private void stepDone(Segment.Step s) {
        inFlight = false;
        entries.add(entry(s.id, "done", true, decisions.get(index).finalDecision.wire, clock.nowMs() - stepStartMono, null, readValue));
        journal(seg.segmentId, s.id, "done", null, null);
        index++;
        later(0, new Runnable() { public void run() { runStep(); } });
    }

    private void stepFailed(Segment.Step s, String why) {
        entries.add(entry(s.id, "failed", false, decisions.get(index).finalDecision.wire, clock.nowMs() - stepStartMono, why, null));
        journal(seg.segmentId, s.id, "failed", null, why);
        end("failed", why);
    }

    private void stopHuman(Segment.Step step) {
        interrupt(step, "human touch");
        journal(seg.segmentId, step == null ? null : step.id, "taken_over", null, "human touch at mono " + touch.lastTouchMs());
        end("stopped_human", "human touch");
    }

    private void end(String outcome, String reason) {
        running = false;
        inFlight = false;
        gen++;
        touch.setListener(null);
        String report = reportJson(seg.segmentId, seg.runId, seg.device, seg.epoch, outcome, entries, reason);
        queue(seg.segmentId, report);
        if (listener != null) listener.onFinished(outcome, report);
    }

    private void queue(String segmentId, String report) {
        journal(segmentId, null, "report_queued", null, null);
        outbox.enqueue(report);
    }

    // ------------------------------------------------------------------ records

    private Map<String, Object> entry(String id, String status, boolean postVerified, String decision, Long ms, String reason, String value) {
        Map<String, Object> m = new LinkedHashMap<String, Object>();
        m.put("id", id);
        m.put("status", status);
        m.put("post_verified", Boolean.valueOf(postVerified));
        m.put("decision", decision);
        m.put("ts_ms", Long.valueOf(clock.wallMs()));
        if (ms != null) m.put("ms", ms);
        if (reason != null) m.put("reason", reason);
        if (value != null) m.put("value", value);
        return m;
    }

    /** The report (crates/segments supervisor::Report, same field order). */
    static String reportJson(String segmentId, String runId, String device, long epoch, String outcome, List<Map<String, Object>> steps, String reason) {
        Map<String, Object> r = new LinkedHashMap<String, Object>();
        r.put("v", Long.valueOf(1));
        r.put("t", "segment_report");
        r.put("segment_id", segmentId);
        r.put("run_id", runId);
        r.put("device", device == null ? "" : device);
        r.put("epoch", Long.valueOf(epoch));
        r.put("outcome", outcome);
        r.put("steps", steps);
        if (reason != null) r.put("reason", reason);
        return Json.write(r);
    }

    private void journal(String segmentId, String stepId, String status, String decision, String reason) {
        Map<String, Object> m = new LinkedHashMap<String, Object>();
        m.put("v", Long.valueOf(1));
        m.put("ts_ms", Long.valueOf(clock.wallMs()));
        m.put("mono_ms", Long.valueOf(clock.nowMs()));
        m.put("segment_id", segmentId);
        if (stepId != null) m.put("step_id", stepId);
        m.put("status", status);
        if (decision != null) m.put("decision", decision);
        if (reason != null) m.put("reason", reason);
        journal.append(m);
    }
}
