package dev.agentroid.helper.runner;

import java.util.List;
import java.util.Map;
import java.util.Random;

/**
 * Wiring 2, item 4: the same stop as HumanTouchTest, but through the REAL {@link HumanTouchFlag}, called the way the
 * 2.15 overlay will call it: {@code flag.touched(SystemClock.elapsedRealtime())} on a qualifying touchscreen
 * ACTION_OUTSIDE, in the runner Clock's nowMs() domain. HumanTouchTest drives a test double; this drives the class the
 * app builds (RunnerWiring.humanTouch), with and without its push listener. The Rust half (hub clock stamping of the
 * getevent feed, the take-over and the 2-minute reclaim) is in crates/bridged/tests/wiring.rs and
 * crates/handover/tests/skew.rs. Not here: the overlay window itself, which needs a phone.
 */
public final class HumanTouchWiringTest {
    /** A flag whose push is hidden, so only the runner's poll can notice a touch. */
    private static final class PollOnly implements HumanTouch {
        private final HumanTouchFlag flag;
        PollOnly(HumanTouchFlag flag) { this.flag = flag; }
        public long lastTouchMs() { return flag.lastTouchMs(); }
        public void setListener(Runnable onTouch) { /* never pushed */ }
    }

    private static final class Wired {
        final FakeClock clock = new FakeClock(Rig.NOW);
        final TestHub hub = new TestHub(Rig.SERIAL, Rig.EPOCH);
        final Fakes.Executor exec;
        final Fakes.Journal journal = new Fakes.Journal();
        final Fakes.Done done = new Fakes.Done(clock);
        final HumanTouchFlag flag = new HumanTouchFlag();
        final SegmentRunner runner;

        Wired(long postDelayMs, boolean push) throws Exception {
            exec = new Fakes.Executor(clock, postDelayMs);
            Fakes.Trust trust = new Fakes.Trust(hub.spki(), Rig.SERIAL, Rig.EPOCH);
            runner = new SegmentRunner(clock, new SegmentVerifier(trust, clock), SafeClasses.bundled(), exec,
                    push ? flag : new PollOnly(flag), journal, new ReportOutbox(new Fakes.Transport(true)), false, done);
        }

        String signed(String id, List<Map<String, Object>> steps) throws Exception {
            return hub.sign(hub.segment(id, "R1", Rig.NOW + 30 * 60_000L, false, steps));
        }

        /** The overlay's call, made at virtual time t on the runner's thread (the app posts it to the Clock). */
        void humanTouchAt(long t) {
            clock.schedule(t - clock.nowMs(), new Runnable() {
                public void run() { flag.touched(clock.nowMs()); }
            });
        }
    }

    @SuppressWarnings("unchecked")
    private static void tenOfTen(boolean push) throws Exception {
        Random rnd = new Random(push ? 2015 : 2016);
        long worst = 0;
        for (int trial = 0; trial < 10; trial++) {
            Wired w = new Wired(300 + rnd.nextInt(1200), push);
            w.runner.submit(w.signed("seg-" + trial, LinkDownTest.fiftySteps()));
            long touchAt = 1_000 + (long) rnd.nextInt(200_000);
            w.humanTouchAt(touchAt);
            w.clock.runIdle(10 * 60_000L);

            String what = "trial " + trial + " (touch at " + touchAt + ", push " + push + ")";
            A.eq("stopped_human", w.done.outcome, what);
            long latency = w.done.at - touchAt;
            A.ok(latency >= 0 && latency <= (push ? 5 : SegmentRunner.TOUCH_POLL_MS), what + ": decision " + latency + " ms after the touch");
            for (int i = 0; i < w.exec.actTimes.size(); i++) {
                A.ok(w.exec.actTimes.get(i).longValue() <= touchAt, what + ": step " + w.exec.acts.get(i) + " acted after the touch");
            }
            A.eq(1, w.journal.count("taken_over"), what + ": taken_over journalled once");
            for (Map<String, Object> s : (List<Map<String, Object>>) Json2.steps(w.done.report)) {
                if ("interrupted".equals(s.get("status"))) A.eq(Boolean.FALSE, s.get("post_verified"), what + ": an interrupted step is not claimed verified");
            }
            worst = Math.max(worst, latency);
        }
        System.out.println("  real HumanTouchFlag, " + (push ? "push" : "poll only") + ": 10/10 stopped; worst decision latency " + worst + " virtual ms");
    }

    public void testTheRealFlagStopsTheSegmentTenOfTenThroughItsPushListener() throws Exception {
        tenOfTen(true);
    }

    public void testTheRealFlagStopsTheSegmentTenOfTenByPollingAloneWithinOnePoll() throws Exception {
        tenOfTen(false);
    }

    public void testATouchBeforeTheSegmentStartsIsInTheFlagButDoesNotStopIt() throws Exception {
        Wired w = new Wired(100, true);
        w.humanTouchAt(2_000);
        w.clock.advance(5_000);
        A.eq(Long.valueOf(2_000), Long.valueOf(w.flag.lastTouchMs()), "the flag holds the earlier touch");
        w.runner.submit(w.signed("seg", java.util.Arrays.asList(TestHub.read("s1"), TestHub.tap("s2"))));
        w.clock.runIdle(60_000);
        A.eq("completed", w.done.outcome, "an older touch is not a new one");
    }

    public void testATouchStopsOneSegmentAndNotTheNextOneThatStartsAfterIt() throws Exception {
        Wired w = new Wired(100, true);
        w.runner.submit(w.signed("seg-1", java.util.Arrays.asList(TestHub.waitMs("w1", 10_000))));
        w.humanTouchAt(3_000);
        w.clock.runIdle(60_000);
        A.eq("stopped_human", w.done.outcome, "first stopped");
        // the person hands the phone back; a new segment starts after the touch and runs
        w.clock.advance(500);
        w.runner.submit(w.signed("seg-2", java.util.Arrays.asList(TestHub.read("s1"), TestHub.tap("s2"))));
        w.clock.runIdle(60_000);
        A.eq("completed", w.done.outcome, "the second segment is not stopped by the first touch");
        A.eq(2, w.done.calls, "two segments finished");
    }

    /** Pulls the steps out of a report JSON (kept tiny: the report is our own, well-formed output). */
    private static final class Json2 {
        @SuppressWarnings("unchecked")
        static Object steps(String report) throws Exception {
            return ((Map<String, Object>) dev.agentroid.helper.core.Json.parse(report)).get("steps");
        }
    }
}
