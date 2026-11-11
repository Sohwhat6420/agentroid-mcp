package dev.agentroid.helper.runner;

import dev.agentroid.helper.core.Json;

import java.time.Instant;
import java.util.List;
import java.util.Map;

/** One runner wired to fakes, a test hub and a virtual clock starting at 2026-10-20T12:00:00Z. */
final class Rig {
    static final long NOW = Instant.parse("2026-10-20T12:00:00Z").toEpochMilli();
    static final String SERIAL = "rig-serial-01";
    static final long EPOCH = 7;

    final FakeClock clock = new FakeClock(NOW);
    final TestHub hub;
    final Fakes.Trust trust;
    final Fakes.Executor exec;
    final Fakes.Touch touch = new Fakes.Touch();
    final Fakes.Transport transport;
    final Fakes.Journal journal = new Fakes.Journal();
    final ReportOutbox outbox;
    final Fakes.Done done = new Fakes.Done(clock);
    final SegmentRunner runner;
    final SegmentVerifier verifier;

    Rig(boolean linkUp, boolean ownedPhone, long postDelayMs) throws Exception {
        this(linkUp, ownedPhone, postDelayMs, null);
    }

    Rig(boolean linkUp, boolean ownedPhone, long postDelayMs, RunnerJournal journalOverride) throws Exception {
        hub = new TestHub(SERIAL, EPOCH);
        trust = new Fakes.Trust(hub.spki(), SERIAL, EPOCH);
        exec = new Fakes.Executor(clock, postDelayMs);
        transport = new Fakes.Transport(linkUp);
        outbox = new ReportOutbox(transport);
        verifier = new SegmentVerifier(trust, clock);
        runner = new SegmentRunner(clock, verifier, SafeClasses.bundled(), exec, touch, journalOverride != null ? journalOverride : journal, outbox, ownedPhone, done);
    }

    String signed(String id, String classification, List<Map<String, Object>> steps) throws Exception {
        return hub.sign(hub.segment(id, classification, NOW + 30 * 60_000L, false, steps));
    }

    /** Submit on the clock thread and run until idle (bounded to maxMs of virtual time). */
    void run(String json, long maxMs) {
        runner.submit(json);
        clock.runIdle(maxMs);
    }

    @SuppressWarnings("unchecked")
    Map<String, Object> report() throws Exception {
        A.ok(done.report != null, "a report was produced");
        return (Map<String, Object>) Json.parse(done.report);
    }

    @SuppressWarnings("unchecked")
    List<Map<String, Object>> reportSteps() throws Exception {
        return (List<Map<String, Object>>) report().get("steps");
    }
}
