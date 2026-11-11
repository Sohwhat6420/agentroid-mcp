package dev.agentroid.helper.runner;

import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.List;

/**
 * Reports wait here while the link is down (a sleeping laptop) and go out, in order, once it is up. The segment itself
 * never waits for the link. A report leaves the queue only when the transport accepted it.
 *
 * Glue (not wired in 2.14): Transport is the link. Until link.json gains a phone-to-hub report frame, the hub pulls
 * with the request runner.report {} after ready and the phone answers with the queued reports (see the report).
 * onLinkUp is called from the link's ready callback. Single-threaded: call on the runner's Clock thread.
 */
public final class ReportOutbox {
    public interface Transport {
        boolean isUp();

        /**
         * True when the hub has the report (acked); false to try again later. IllegalArgumentException means this
         * report can never be sent: it is dropped and counted as rejected, and the ones behind it still go.
         */
        boolean send(String reportJson);
    }

    private final Transport transport;
    private final ArrayDeque<String> queue = new ArrayDeque<String>();
    private final List<String> delivered = new ArrayList<String>();
    private int rejected;

    public ReportOutbox(Transport transport) {
        this.transport = transport;
    }

    public void enqueue(String reportJson) {
        queue.add(reportJson);
        flush();
    }

    /** The link reached ready again. */
    public void onLinkUp() { flush(); }

    public int pending() { return queue.size(); }

    /** Count of reports the transport has accepted. */
    public int deliveredCount() { return delivered.size(); }

    /** Count of reports the transport refused for good (they were dropped, not delivered). */
    public int rejectedCount() { return rejected; }

    private void flush() {
        while (!queue.isEmpty() && transport.isUp()) {
            String r = queue.peek();
            boolean sent;
            try {
                sent = transport.send(r);
            } catch (IllegalArgumentException e) {
                queue.poll();
                rejected++;
                continue;
            }
            if (!sent) return;
            delivered.add(queue.poll());
        }
    }
}
