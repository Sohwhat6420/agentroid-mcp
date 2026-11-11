package dev.agentroid.helper.app;

import android.content.Context;
import android.os.SystemClock;

import dev.agentroid.helper.core.Clock;
import dev.agentroid.helper.core.Dispatcher;
import dev.agentroid.helper.link.LinkTrust;
import dev.agentroid.helper.link.net.ExecutorClock;
import dev.agentroid.helper.runner.HumanTouchFlag;
import dev.agentroid.helper.runner.ReportOutbox;
import dev.agentroid.helper.runner.ReportStore;
import dev.agentroid.helper.runner.RunnerMethods;
import dev.agentroid.helper.runner.SegmentRunner;
import dev.agentroid.helper.runner.SegmentVerifier;

/**
 * The runner's phone-side parts, built once: a Clock on SystemClock.elapsedRealtime (the
 * domain HumanTouch promises), the writable human-touch flag the 2.15 overlay sets, and a SegmentVerifier over the
 * link's own HubStore. Wiring 2 (25 Oct) added the report store (the outbox's pull-model transport) and
 * {@link #registerMethods}, which routes the link's runner.segment and runner.report requests to a SegmentRunner (tested
 * on the JVM in RunnerMethodsTest). Still CP-D, because it needs a phone: the StepExecutor over the accessibility
 * service that a SegmentRunner is built with, and the one call to registerMethods from LinkService once it has both
 * its Dispatcher and that runner.
 */
public final class RunnerWiring {
    /** elapsedRealtime for nowMs; scheduling on one ExecutorClock thread (the Clock contract). */
    static final class ElapsedClock implements Clock {
        private final ExecutorClock exec = new ExecutorClock("agentroid-runner");

        @Override public long nowMs() { return SystemClock.elapsedRealtime(); }

        @Override public long wallMs() { return System.currentTimeMillis(); }

        @Override public Task schedule(long delayMs, Runnable r) { return exec.schedule(delayMs, r); }
    }

    private static RunnerWiring instance;

    public final Clock clock;
    public final HumanTouchFlag humanTouch;
    public final SegmentVerifier verifier;
    /** Finished reports wait here for the hub's runner.report; bounded by age and count. */
    public final ReportStore reports;

    private RunnerWiring(Context app) {
        clock = new ElapsedClock();
        humanTouch = new HumanTouchFlag();
        // the overlay (in the accessibility service) reports human touches here
        dev.agentroid.helper.core.PortRegistry.shared().addHumanTouchListener(new dev.agentroid.helper.core.PortRegistry.HumanTouchListener() {
            @Override public void touched(long elapsedRealtimeMs) { humanTouch.touched(elapsedRealtimeMs); }
        });
        verifier = new SegmentVerifier(LinkTrust.open(app), clock);
        reports = new ReportStore(clock);
    }

    /** The outbox a SegmentRunner must be built with, so its reports land in {@link #reports}. */
    public ReportOutbox newOutbox() {
        return new ReportOutbox(reports);
    }

    /**
     * Route runner.segment and runner.report on the link's Dispatcher to a runner built with {@link #clock},
     * {@link #verifier}, {@link #humanTouch} and {@link #newOutbox()}. The hop puts every submit on the runner's own clock thread.
     */
    public void registerMethods(Dispatcher dispatcher, SegmentRunner runner) {
        RunnerMethods.register(dispatcher, new RunnerMethods.ClockHop(clock, runner, 5_000), reports);
    }

    public static synchronized RunnerWiring get(Context ctx) {
        if (instance == null) instance = new RunnerWiring(ctx.getApplicationContext());
        return instance;
    }
}
