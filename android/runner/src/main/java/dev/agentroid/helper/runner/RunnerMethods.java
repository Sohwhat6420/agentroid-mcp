package dev.agentroid.helper.runner;

import dev.agentroid.helper.core.Clock;
import dev.agentroid.helper.core.Dispatcher;
import dev.agentroid.helper.core.ErrorCodes;
import dev.agentroid.helper.core.HandlerException;
import dev.agentroid.helper.core.Json;

import java.util.LinkedHashMap;
import java.util.Map;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

/**
 * The two link methods that reach the segment runner (wiring 2, item 1). The hub's segment frame is recast by
 * LinkProto.segmentAsRequest as a request to {@code runner.segment} with the signed segment as params, and the hub
 * pulls the report with {@code runner.report {segment_id}}. Both go through the ordinary Dispatcher, so the auth gate
 * (READY only), the request-id dedupe (a resent segment is answered from memory and never runs twice) and the epoch
 * fence in LinkClient all apply before a handler is reached.
 *
 * <p>{@code runner.segment} answers at once with the admission: {@code {accepted, outcome, segment_id}} when the
 * segment was taken (it then runs on the runner's own clock thread, long after the answer), or the same with
 * {@code accepted:false} and a {@code code}/{@code reason} when the phone refused it. A refusal is an answer, not a
 * JSON-RPC error, so the link caches it like any other and the hub reads it as a phone refusal. Nothing here decides
 * whether a segment is safe: the verifier and the phone's own re-classification do that inside SegmentRunner.submit.
 *
 * <p>What this cannot do without a phone: the StepExecutor that SegmentRunner runs steps through is accessibility
 * glue (see StepExecutor), and the call that registers these methods on LinkService's Dispatcher lives in the app
 * wiring. Everything between those two is exercised on the JVM with fakes.
 */
public final class RunnerMethods {
    private RunnerMethods() { }

    /** Hands a signed segment to the SegmentRunner on its own thread and returns what the runner decided. */
    public interface Submitter {
        Admission submit(String signedSegmentJson) throws HandlerException;
    }

    /**
     * A Submitter that hops from the link's thread onto the runner's Clock thread (SegmentRunner is single-threaded
     * on its Clock) and waits for the admission, bounded. On a timeout the queued submit is cancelled if it has not
     * started yet; if it already started, the segment runs and the hub is told the call may still run.
     */
    public static final class ClockHop implements Submitter {
        private final Clock clock;
        private final SegmentRunner runner;
        private final long timeoutMs;

        public ClockHop(Clock clock, SegmentRunner runner, long timeoutMs) {
            this.clock = clock;
            this.runner = runner;
            this.timeoutMs = timeoutMs;
        }

        @Override
        public Admission submit(final String json) throws HandlerException {
            final Admission[] out = new Admission[1];
            final RuntimeException[] failed = new RuntimeException[1];
            final CountDownLatch done = new CountDownLatch(1);
            Clock.Task task = clock.schedule(0, new Runnable() {
                public void run() {
                    try {
                        out[0] = runner.submit(json);
                    } catch (RuntimeException e) {
                        failed[0] = e;
                    } finally {
                        done.countDown();
                    }
                }
            });
            try {
                if (!done.await(timeoutMs, TimeUnit.MILLISECONDS)) {
                    task.cancel();
                    // TRANSIENT: not remembered anywhere, so it cannot pin the segment id; the hub reads it as
                    // "outcome unknown" (the submit may have started the segment just before the timeout fired)
                    Map<String, Object> why = reason("runner_timeout");
                    why.put("outcome_unknown", Boolean.TRUE);
                    throw new HandlerException(ErrorCodes.TRANSIENT, "the runner did not answer in time", why);
                }
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                task.cancel();
                throw new HandlerException(ErrorCodes.DEVICE_ERROR, "interrupted", reason("interrupted"));
            }
            if (failed[0] != null) {
                // class name only: exception text could carry screen content
                throw new HandlerException(ErrorCodes.INTERNAL_ERROR, "runner failed: " + failed[0].getClass().getSimpleName(), null);
            }
            return out[0];
        }
    }

    /** Register {@code runner.segment} (ACT: gated, deduped by id) and {@code runner.report} (OBSERVE: gated). */
    public static void register(Dispatcher d, final Submitter submitter, final ReportStore reports) {
        d.register("runner.segment", Dispatcher.Kind.ACT, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> params) throws HandlerException {
                if (params == null || params.isEmpty()) {
                    throw new HandlerException(ErrorCodes.INVALID_PARAMS, "runner.segment needs the signed segment as params", null);
                }
                Admission a = submitter.submit(Json.write(params));
                if (!a.accepted && "busy".equals(a.outcome)) {
                    // Nothing ran and nothing was queued, so there is no report to pull: a transient refusal that no
                    // cache remembers, and the same segment id may be sent again once the runner is free.
                    throw new HandlerException(ErrorCodes.TRANSIENT, "busy: " + a.reason, reason("busy"));
                }
                Map<String, Object> r = new LinkedHashMap<String, Object>();
                r.put("accepted", Boolean.valueOf(a.accepted));
                r.put("outcome", a.outcome);
                r.put("segment_id", a.segmentId);
                if (!a.accepted) {
                    r.put("code", a.code);
                    r.put("reason", a.reason);
                }
                return r;
            }
        });
        d.register("runner.report", Dispatcher.Kind.OBSERVE, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> params) throws HandlerException {
                Object id = params == null ? null : params.get("segment_id");
                if (!(id instanceof String)) {
                    throw new HandlerException(ErrorCodes.INVALID_PARAMS, "runner.report needs segment_id", null);
                }
                String report = reports.get((String) id);
                if (report == null) {
                    // TRANSIENT: "not ready" must not be remembered, or the hub's fixed report id would answer
                    // "no report" for ever, even after the segment has finished and filed its report
                    throw new HandlerException(ErrorCodes.TRANSIENT, "no report for segment " + id + " (yet)", reason("no_report"));
                }
                try {
                    return Json.parse(report);
                } catch (Json.ParseException e) {
                    throw new HandlerException(ErrorCodes.INTERNAL_ERROR, "stored report unreadable", null);
                }
            }
        });
    }

    private static Map<String, Object> reason(String why) {
        Map<String, Object> m = new LinkedHashMap<String, Object>();
        m.put("reason", why);
        return m;
    }
}
