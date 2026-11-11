package dev.agentroid.helper.runner;

import dev.agentroid.helper.core.Clock;
import dev.agentroid.helper.core.Dispatcher;
import dev.agentroid.helper.core.ErrorCodes;
import dev.agentroid.helper.core.HandlerException;
import dev.agentroid.helper.core.Json;
import dev.agentroid.helper.core.LinkSession;
import dev.agentroid.helper.core.StubChallenge;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Executors;
import java.util.concurrent.ScheduledExecutorService;
import java.util.concurrent.ScheduledFuture;
import java.util.concurrent.TimeUnit;

/**
 * Wiring 2, item 1: a signed segment arrives as a runner.segment request through the real Dispatcher, reaches the
 * real SegmentRunner (verifier, re-classification, step loop) and the hub can pull the report with runner.report.
 * Fakes stand in for the phone only where a phone is needed: the StepExecutor (accessibility) and the clock.
 */
public final class RunnerMethodsTest {
    /** The runner behind a Dispatcher, wired the way the app will wire it, minus the accessibility executor. */
    private static final class Wired {
        final FakeClock clock = new FakeClock(Rig.NOW);
        final TestHub hub = new TestHub(Rig.SERIAL, Rig.EPOCH);
        final Fakes.Trust trust = new Fakes.Trust(hub.spki(), Rig.SERIAL, Rig.EPOCH);
        final Fakes.Executor exec = new Fakes.Executor(clock, 300);
        final Fakes.Touch touch = new Fakes.Touch();
        final Fakes.Journal journal = new Fakes.Journal();
        final ReportStore reports = new ReportStore(clock);
        final Fakes.Done done = new Fakes.Done(clock);
        final SegmentRunner runner;
        final LinkSession session = new LinkSession(new StubChallenge());
        final Dispatcher dispatcher = new Dispatcher(session);
        int submits;

        Wired() throws Exception {
            runner = new SegmentRunner(clock, new SegmentVerifier(trust, clock), SafeClasses.bundled(), exec, touch, journal,
                    new ReportOutbox(reports), false, done);
            RunnerMethods.register(dispatcher, new RunnerMethods.Submitter() {
                public Admission submit(String json) {
                    submits++;
                    return runner.submit(json);
                }
            }, reports);
            session.linkReady();
        }

        String signed(String id, List<Map<String, Object>> steps) throws Exception {
            return hub.sign(hub.segment(id, "R1", Rig.NOW + 30 * 60_000L, false, steps));
        }

        /** One request through the Dispatcher exactly as LinkClient hands it over, and the parsed answer. */
        @SuppressWarnings("unchecked")
        Map<String, Object> call(String requestId, String method, Object params) throws Exception {
            Map<String, Object> req = new LinkedHashMap<String, Object>();
            req.put("jsonrpc", "2.0");
            req.put("id", requestId);
            req.put("method", method);
            req.put("params", params);
            byte[] out = dispatcher.handleFrame(Json.write(req).getBytes(StandardCharsets.UTF_8));
            return (Map<String, Object>) Json.parse(new String(out, StandardCharsets.UTF_8));
        }

        @SuppressWarnings("unchecked")
        Map<String, Object> segment(String requestId, String signedJson) throws Exception {
            return call(requestId, "runner.segment", Json.parse(signedJson));
        }
    }

    @SuppressWarnings("unchecked")
    private static Map<String, Object> result(Map<String, Object> response) {
        A.ok(response.get("error") == null, "no error expected, got " + response.get("error"));
        return (Map<String, Object>) response.get("result");
    }

    @SuppressWarnings("unchecked")
    private static Map<String, Object> error(Map<String, Object> response) {
        A.ok(response.get("result") == null, "an error expected, got " + response.get("result"));
        return (Map<String, Object>) response.get("error");
    }

    private static List<Map<String, Object>> tapThenRead() {
        List<Map<String, Object>> steps = new ArrayList<Map<String, Object>>();
        steps.add(TestHub.tap("s1"));
        steps.add(TestHub.read("s2"));
        return steps;
    }

    public void testASignedSegmentArrivingAsARunnerSegmentRequestRunsAndItsReportIsPulledWithRunnerReport() throws Exception {
        Wired w = new Wired();
        Map<String, Object> r = result(w.segment("seg-1", w.signed("seg-1", tapThenRead())));
        A.eq(Boolean.TRUE, r.get("accepted"), "the phone took the segment");
        A.eq("started", r.get("outcome"), "outcome");
        A.eq("seg-1", r.get("segment_id"), "segment id");
        A.eq(0, w.done.calls, "the answer comes before the segment has finished: it runs on after the answer");
        // no report while it runs
        Map<String, Object> early = error(w.call("rep-0", "runner.report", params("segment_id", "seg-1")));
        A.eq(Long.valueOf(ErrorCodes.TRANSIENT), early.get("code"), "no report yet: a transient answer, which no cache keeps");
        A.eq("no_report", ((Map<?, ?>) early.get("data")).get("reason"), "and it says why");

        w.clock.runIdle(60_000);
        A.eq("completed", w.done.outcome, "the runner ran it to the end");
        A.eq(Arrays.asList("s1"), w.exec.acts, "the tap was performed through the executor, the read was not an action");

        Map<String, Object> rep = result(w.call("rep-1", "runner.report", params("segment_id", "seg-1")));
        A.eq("segment_report", rep.get("t"), "it is a segment report");
        A.eq("seg-1", rep.get("segment_id"), "report is for the segment");
        A.eq("completed", rep.get("outcome"), "report outcome");
        A.eq(Rig.SERIAL, rep.get("device"), "report device");
        A.eq(2, ((List<?>) rep.get("steps")).size(), "both steps reported");
        // asking again gives the same answer (the hub may ask twice after a dropped link)
        A.eq(rep, result(w.call("rep-2", "runner.report", params("segment_id", "seg-1"))), "the report is stable");
    }

    public void testAPhoneRefusalIsAnAnswerNotAnErrorAndTheRefusedReportCanStillBePulled() throws Exception {
        Wired w = new Wired();
        // the segment is tampered with after signing: the phone's verifier must refuse before any step
        String tampered = w.signed("seg-bad", tapThenRead()).replace("\"s2\"", "\"s9\"");
        Map<String, Object> r = result(w.segment("seg-bad", tampered));
        A.eq(Boolean.FALSE, r.get("accepted"), "refused");
        A.eq("refused", r.get("outcome"), "outcome");
        A.eq("bad_signature", r.get("code"), "the verifier's own code");
        A.eq("seg-bad", r.get("segment_id"), "segment id");
        A.eq(0, w.exec.acts.size(), "nothing ran");
        Map<String, Object> rep = result(w.call("rep-bad", "runner.report", params("segment_id", "seg-bad")));
        A.eq("refused", rep.get("outcome"), "the queued report says refused");
        A.eq("bad_signature", rep.get("reason"), "with the reason");
    }

    public void testASegmentForAnotherDeviceOrAStaleEpochIsRefusedByThePhoneNotTrusted() throws Exception {
        Wired w = new Wired();
        w.trust.epoch = Rig.EPOCH + 1; // the phone has moved on to a newer hub epoch
        Map<String, Object> r = result(w.segment("seg-old", w.signed("seg-old", tapThenRead())));
        A.eq(Boolean.FALSE, r.get("accepted"), "stale epoch is refused");
        A.eq("stale_epoch", r.get("code"), "code");
        w.trust.epoch = Rig.EPOCH;
        w.trust.serial = "some-other-phone";
        r = result(w.segment("seg-other", w.signed("seg-other", tapThenRead())));
        A.eq("wrong_device", r.get("code"), "wrong device");
        A.eq(0, w.exec.acts.size(), "nothing ran");
    }

    public void testNothingRunsUntilTheLinkIsReadyAndAResentSegmentIsAnsweredFromMemoryNeverRunTwice() throws Exception {
        Wired w = new Wired();
        String signed = w.signed("seg-r", tapThenRead());
        w.session.onDisconnect(); // pending_auth: the link is not authenticated
        Map<String, Object> e = error(w.segment("seg-r", signed));
        A.eq(Long.valueOf(ErrorCodes.AUTH_REQUIRED), e.get("code"), "the auth gate holds");
        A.eq(0, w.submits, "the handler was not reached");
        A.eq(0, w.exec.acts.size(), "nothing ran");

        w.session.linkReady();
        Map<String, Object> first = w.segment("seg-r", signed);
        A.eq(Boolean.TRUE, result(first).get("accepted"), "taken once the link is ready");
        Map<String, Object> again = w.segment("seg-r", signed); // the hub re-sent after a reconnect
        A.eq(first, again, "answered from memory");
        A.eq(1, w.submits, "the runner saw it once");
        w.clock.runIdle(60_000);
        A.eq(1, w.exec.acts.size(), "the tap happened once");
        // the same request id carrying a different segment is refused by the Dispatcher
        Map<String, Object> other = error(w.segment("seg-r", w.signed("seg-r2", tapThenRead())));
        A.eq(Long.valueOf(ErrorCodes.INVALID_REQUEST), other.get("code"), "id reuse");
    }

    public void testASecondSegmentWhileOneIsRunningIsRefusedAsBusy() throws Exception {
        Wired w = new Wired();
        List<Map<String, Object>> slow = new ArrayList<Map<String, Object>>();
        slow.add(TestHub.waitMs("w1", 10_000));
        A.eq(Boolean.TRUE, result(w.segment("seg-a", w.signed("seg-a", slow))).get("accepted"), "first taken");
        // busy is a transient error, not an answer: nothing remembers it, and no report is queued for it
        Map<String, Object> second = error(w.segment("seg-b", w.signed("seg-b", tapThenRead())));
        A.eq(Long.valueOf(ErrorCodes.TRANSIENT), second.get("code"), "second refused as transient");
        A.eq("busy", ((Map<?, ?>) second.get("data")).get("reason"), "busy");
        A.eq(0, w.exec.acts.size(), "the second one never acted");
        A.eq(0, w.reports.size(), "no report was queued for the busy refusal");
        w.clock.runIdle(60_000);
        A.eq("completed", w.done.outcome, "the first still finishes");
        // the busy answer did not pin the id: the same request id and segment run once the runner is free
        Map<String, Object> again = result(w.segment("seg-b", w.signed("seg-b", tapThenRead())));
        A.eq(Boolean.TRUE, again.get("accepted"), "seg-b is taken the second time");
        w.clock.runIdle(60_000);
        A.eq("completed", w.done.outcome, "and it ran");
        A.eq(3, w.submits, "the runner was asked three times: seg-a, seg-b while busy, seg-b again");
    }

    /**
     * Review round 2, defect B: a runner timeout leaves the segment started, with a TRANSIENT answer nobody remembers.
     * If the same segment id then arrives again (after the first run finished, or while it still runs) the phone must
     * refuse it with an ordinary, remembered answer and run nothing twice.
     */
    public void testAResentSegmentAfterATimeoutIsRefusedAsADuplicateAndRunsOnce() throws Exception {
        Wired w = new Wired();
        String signed = w.signed("seg-t", tapThenRead());
        // the call that "timed out" at the hub: the runner did start the segment, the hub never saw the answer
        A.eq(Boolean.TRUE, Boolean.valueOf(w.runner.submit(signed).accepted), "the segment started");
        // while it still runs: a duplicate, not busy (busy would tell the hub nothing ran)
        Map<String, Object> mid = result(w.segment("resend-1", signed));
        A.eq(Boolean.FALSE, mid.get("accepted"), "refused while running");
        A.eq("duplicate_segment", mid.get("code"), "as a duplicate, not busy");
        w.clock.runIdle(60_000);
        A.eq("completed", w.done.outcome, "the first run finished");
        // after it finished: still refused, with an answer the link remembers (a result, not a TRANSIENT error)
        Map<String, Object> after = w.segment("resend-2", signed);
        Map<String, Object> r = result(after);
        A.eq(Boolean.FALSE, r.get("accepted"), "refused after the first run finished");
        A.eq("duplicate_segment", r.get("code"), "duplicate");
        A.eq(after, w.segment("resend-2", signed), "and the same request id is answered from memory");
        A.eq(Arrays.asList("s1"), w.exec.acts, "the tap happened once");
        A.eq(1, w.reports.size(), "no second report was queued over the first");
        A.eq("completed", result(w.call("rep-t", "runner.report", params("segment_id", "seg-t"))).get("outcome"), "the first run's report is intact");
    }

    public void testASegmentThatWasOnlyRefusedBusyOrByTheVerifierCanBeSentAgain() throws Exception {
        Wired w = new Wired();
        List<Map<String, Object>> slow = new ArrayList<Map<String, Object>>();
        slow.add(TestHub.waitMs("w1", 10_000));
        result(w.segment("a", w.signed("seg-a", slow)));
        A.eq(Long.valueOf(ErrorCodes.TRANSIENT), error(w.segment("b-1", w.signed("seg-b", tapThenRead()))).get("code"), "busy");
        w.clock.runIdle(60_000);
        A.eq(Boolean.TRUE, result(w.segment("b-2", w.signed("seg-b", tapThenRead()))).get("accepted"), "seg-b was never started, so it runs now");
        w.clock.runIdle(60_000);
        A.eq(Arrays.asList("s1"), w.exec.acts, "and runs once");
        // a verifier refusal did not start anything either
        String tampered = w.signed("seg-bad", tapThenRead()).replace("\"s2\"", "\"s9\"");
        A.eq("bad_signature", result(w.segment("bad-1", tampered)).get("code"), "refused");
        A.eq(Boolean.TRUE, result(w.segment("bad-2", w.signed("seg-bad", tapThenRead()))).get("accepted"), "the genuine segment with that id runs");
    }

    public void testTheRememberedSegmentIdsAreBoundedSoTheOldestMayRunAgain() throws Exception {
        Wired w = new Wired();
        List<Map<String, Object>> one = new ArrayList<Map<String, Object>>();
        one.add(TestHub.read("s1"));
        String first = w.signed("seg-0", one);
        A.eq(Boolean.TRUE, result(w.segment("r-0", first)).get("accepted"), "first");
        w.clock.runIdle(60_000);
        A.eq("duplicate_segment", result(w.segment("r-0b", first)).get("code"), "remembered");
        for (int i = 1; i <= SegmentRunner.ADMITTED_KEPT; i++) {
            A.eq(Boolean.TRUE, result(w.segment("r-" + i, w.signed("seg-" + i, one))).get("accepted"), "segment " + i);
            w.clock.runIdle(60_000);
        }
        A.eq(Boolean.TRUE, result(w.segment("r-again", first)).get("accepted"), "the oldest id was forgotten once more than " + SegmentRunner.ADMITTED_KEPT + " newer ones started");
    }

    public void testBadParamsAreInvalidParamsAndAnUnknownSegmentReportIsADeviceError() throws Exception {
        Wired w = new Wired();
        A.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), error(w.call("p-1", "runner.segment", new LinkedHashMap<String, Object>())).get("code"), "empty params");
        A.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), error(w.call("p-2", "runner.report", new LinkedHashMap<String, Object>())).get("code"), "no segment_id");
        A.eq(Long.valueOf(ErrorCodes.TRANSIENT), error(w.call("p-3", "runner.report", params("segment_id", "never-sent"))).get("code"), "unknown segment: nothing to report");
        A.eq(0, w.submits, "no handler call reached the runner");
    }

    // ------------------------------------------------------------------ the thread hop (real threads)

    /** A Clock on one real thread, with wall time pinned to the fixtures' day so the signed segments are not expired. */
    private static final class ThreadClock implements Clock {
        final ScheduledExecutorService ex = Executors.newSingleThreadScheduledExecutor(new java.util.concurrent.ThreadFactory() {
            public Thread newThread(Runnable r) {
                Thread t = new Thread(r, "runner-clock-test");
                t.setDaemon(true);
                return t;
            }
        });
        final long t0 = System.nanoTime();
        public long nowMs() { return (System.nanoTime() - t0) / 1_000_000L; }
        public long wallMs() { return Rig.NOW + nowMs(); }
        public Task schedule(long delayMs, final Runnable r) {
            final ScheduledFuture<?> f = ex.schedule(r, Math.max(0, delayMs), TimeUnit.MILLISECONDS);
            return new Task() { public void cancel() { f.cancel(false); } };
        }
    }

    private static final class OneReadExecutor implements StepExecutor {
        public int count(Map<String, Object> select) { return 1; }
        public String read(Map<String, Object> select) { return "Battery"; }
        public String act(Segment.Step step) { return null; }
        public String setting(String namespace, String key) { return null; }
    }

    public void testTheClockHopRunsTheSegmentOnTheRunnersOwnThreadAndAnswersTheCallerOnItsThread() throws Exception {
        final ThreadClock clock = new ThreadClock();
        try {
            TestHub hub = new TestHub(Rig.SERIAL, Rig.EPOCH);
            Fakes.Trust trust = new Fakes.Trust(hub.spki(), Rig.SERIAL, Rig.EPOCH);
            final List<String> threads = new ArrayList<String>();
            RunnerJournal journal = new RunnerJournal() {
                public void append(Map<String, Object> r) { synchronized (threads) { threads.add(Thread.currentThread().getName()); } }
            };
            ReportStore reports = new ReportStore(clock);
            SegmentRunner runner = new SegmentRunner(clock, new SegmentVerifier(trust, clock), SafeClasses.bundled(), new OneReadExecutor(),
                    new Fakes.Touch(), journal, new ReportOutbox(reports), false, new Fakes.Done(new FakeClock(0)));
            LinkSession session = new LinkSession(new StubChallenge());
            session.linkReady();
            Dispatcher d = new Dispatcher(session);
            RunnerMethods.register(d, new RunnerMethods.ClockHop(clock, runner, 5_000), reports);

            List<Map<String, Object>> steps = new ArrayList<Map<String, Object>>();
            steps.add(TestHub.read("s1"));
            String signed = hub.sign(hub.segment("seg-hop", "R0", Rig.NOW + 30 * 60_000L, false, steps));
            Map<String, Object> req = new LinkedHashMap<String, Object>();
            req.put("jsonrpc", "2.0");
            req.put("id", "seg-hop");
            req.put("method", "runner.segment");
            req.put("params", Json.parse(signed));
            String caller = Thread.currentThread().getName();
            byte[] out = d.handleFrame(Json.write(req).getBytes(StandardCharsets.UTF_8));
            Map<?, ?> resp = (Map<?, ?>) Json.parse(new String(out, StandardCharsets.UTF_8));
            A.ok(resp.get("error") == null, "no error: " + resp.get("error"));
            A.eq(Boolean.TRUE, ((Map<?, ?>) resp.get("result")).get("accepted"), "taken");
            long until = System.currentTimeMillis() + 5_000;
            while (reports.get("seg-hop") == null && System.currentTimeMillis() < until) Thread.sleep(5);
            A.ok(reports.get("seg-hop") != null, "the report was filed by the runner");
            synchronized (threads) {
                A.ok(!threads.isEmpty(), "the runner journalled");
                for (String t : threads) A.eq("runner-clock-test", t, "every runner step ran on the runner's thread, not on " + caller);
            }
        } finally {
            clock.ex.shutdownNow();
        }
    }

    public void testTheClockHopGivesUpWithATransientOutcomeUnknownErrorAndCancelsWhenTheRunnerNeverAnswers() throws Exception {
        final boolean[] cancelled = new boolean[1];
        Clock stuck = new Clock() {
            public long nowMs() { return 0; }
            public long wallMs() { return Rig.NOW; }
            public Task schedule(long delayMs, Runnable r) {
                return new Task() { public void cancel() { cancelled[0] = true; } };
            }
        };
        RunnerMethods.ClockHop hop = new RunnerMethods.ClockHop(stuck, null, 50);
        HandlerException e = A.throwsA(HandlerException.class, new A.Body() {
            public void run() throws Exception { hop.submit("{}"); }
        }, "a stuck runner is an error, not a hang");
        A.eq(ErrorCodes.TRANSIENT, e.code, "transient: not remembered by any cache");
        A.eq("runner_timeout", ((Map<?, ?>) e.data).get("reason"), "reason");
        A.eq(Boolean.TRUE, ((Map<?, ?>) e.data).get("outcome_unknown"), "the hub is told the segment may be running");
        A.ok(cancelled[0], "the queued submit was cancelled so it cannot start late");
    }

    // ------------------------------------------------------------------ the report store is bounded

    private static String report(String id) {
        return SegmentRunner.reportJson(id, "run-1", Rig.SERIAL, Rig.EPOCH, "completed", new ArrayList<Map<String, Object>>(), null);
    }

    public void testReportsAgeOutAndTheOldestGoFirstBeyondTheCountAndFreshOnesStay() {
        FakeClock clock = new FakeClock(Rig.NOW);
        ReportStore store = new ReportStore(clock, 3, 10_000);
        store.send(report("a"));
        clock.advance(4_000);
        store.send(report("b"));
        clock.advance(4_000);
        store.send(report("c"));
        A.eq(3, store.size(), "three kept");
        store.send(report("d")); // over the count: the oldest (a) goes, though it is not yet too old
        A.ok(store.get("a") == null, "a went by count");
        A.ok(store.get("b") != null && store.get("c") != null && store.get("d") != null, "the rest stay");
        clock.advance(7_000); // t = 15 s: b is 11 s old, c and d are 7 s
        A.ok(store.get("b") == null, "b went by age");
        A.ok(store.get("c") != null && store.get("d") != null, "fresh ones stay");
        clock.advance(4_000); // t = 19 s: c and d are 11 s old
        A.eq(2, store.prune(), "an explicit prune drops the two that aged out");
        A.eq(0, store.size(), "nothing left");
    }

    public void testAReportThatCannotBeFiledIsRejectedNotCountedAsDeliveredAndDoesNotBlockTheOnesBehindIt() {
        FakeClock clock = new FakeClock(Rig.NOW);
        ReportStore store = new ReportStore(clock);
        ReportOutbox outbox = new ReportOutbox(store);
        outbox.enqueue("this is not json");
        outbox.enqueue("{\"v\":1,\"no_segment_id\":true}");
        outbox.enqueue(report("good"));
        A.eq(2, store.unfiled(), "both unfileable reports were refused by the store");
        A.eq(2, outbox.rejectedCount(), "and counted as rejected by the outbox");
        A.eq(1, outbox.deliveredCount(), "only the good one counts as delivered");
        A.eq(0, outbox.pending(), "nothing is stuck at the head of the queue");
        A.eq(1, store.size(), "one report is filed");
        A.ok(store.get("good") != null, "the good one can be pulled");
    }

    private static Map<String, Object> params(String k, Object v) {
        Map<String, Object> m = new LinkedHashMap<String, Object>();
        m.put(k, v);
        return m;
    }
}
