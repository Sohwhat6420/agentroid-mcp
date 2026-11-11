package dev.agentroid.helper.runner;

import dev.agentroid.helper.link.net.TrustStore;

import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/** Test doubles: trust store, executor, touch flag, transport, journal. */
final class Fakes {
    private Fakes() { }

    /** The link's TrustStore in memory. The runner must only read it. */
    static final class Trust implements TrustStore {
        byte[] spki;
        String serial;
        long epoch;
        int writes;
        Trust(byte[] spki, String serial, long epoch) { this.spki = spki; this.serial = serial; this.epoch = epoch; }
        public byte[] hubKeySpki() { return spki; }
        public String serial() { return serial; }
        public long maxEpoch() { return epoch; }
        public void setMaxEpoch(long e) { writes++; epoch = e; }
    }

    /**
     * A screen model good enough for the runner: every step selector matches exactly one node, unless overridden in
     * counts; the post-condition of the LAST acted step starts to hold postDelayMs (per step id, else defaultDelayMs)
     * after act(). Settings writes land at the same moment.
     */
    static final class Executor implements StepExecutor {
        final FakeClock clock;
        long defaultDelayMs;
        final Map<String, Long> delays = new HashMap<String, Long>();
        final Map<String, Integer> counts = new HashMap<String, Integer>();
        final List<String> acts = new ArrayList<String>();
        final List<Long> actTimes = new ArrayList<Long>();
        int queries;
        private Segment.Step last;
        private long readyAt;

        Executor(FakeClock clock, long defaultDelayMs) { this.clock = clock; this.defaultDelayMs = defaultDelayMs; }

        private boolean ready() { return clock.nowMs() >= readyAt; }

        private boolean isPostSelect(Map<String, Object> sel) {
            return last != null && last.post.select != null && last.post.select.equals(sel);
        }

        public int count(Map<String, Object> sel) {
            queries++;
            if (isPostSelect(sel)) {
                boolean r = ready();
                return last.post.kind.equals("absent") ? (r ? 0 : 1) : (r ? 1 : 0);
            }
            Integer c = counts.get(Canonical.write(sel));
            return c == null ? 1 : c.intValue();
        }

        public String read(Map<String, Object> sel) {
            queries++;
            if (isPostSelect(sel) && last.post.text != null) return ready() ? last.post.text : "(old)";
            return "text of " + Canonical.write(sel);
        }

        public String act(Segment.Step s) {
            acts.add(s.id);
            actTimes.add(Long.valueOf(clock.nowMs()));
            last = s;
            Long d = delays.get(s.id);
            readyAt = clock.nowMs() + (d != null ? d.longValue() : defaultDelayMs);
            return null;
        }

        public String setting(String ns, String key) {
            queries++;
            if (last != null && "settings_set".equals(last.kind) && key.equals(last.key) && ready()) return String.valueOf(last.value);
            return "(old)";
        }
    }

    static final class Touch implements HumanTouch {
        long last = NEVER;
        Runnable listener;
        int listenerSets;
        public long lastTouchMs() { return last; }
        public void setListener(Runnable r) { listener = r; listenerSets++; }

        /** A human touches at virtual time t; push=true also fires the listener (the overlay's push path). */
        void touchAt(final FakeClock clock, long t, final boolean push) {
            clock.schedule(t - clock.nowMs(), new Runnable() {
                public void run() {
                    last = clock.nowMs();
                    if (push && listener != null) listener.run();
                }
            });
        }
    }

    static final class Transport implements ReportOutbox.Transport {
        boolean up;
        final List<String> received = new ArrayList<String>();
        Transport(boolean up) { this.up = up; }
        public boolean isUp() { return up; }
        public boolean send(String r) {
            if (!up) return false;
            received.add(r);
            return true;
        }
    }

    static final class Journal implements RunnerJournal {
        final List<Map<String, Object>> records = new ArrayList<Map<String, Object>>();
        public void append(Map<String, Object> r) { records.add(r); }

        int count(String status) {
            int n = 0;
            for (Map<String, Object> r : records) if (status.equals(r.get("status"))) n++;
            return n;
        }
    }

    /** Captures the finish callback and when it fired. */
    static final class Done implements SegmentRunner.Listener {
        final FakeClock clock;
        String outcome, report;
        long at = -1;
        int calls;
        Done(FakeClock clock) { this.clock = clock; }
        public void onFinished(String o, String r) { outcome = o; report = r; at = clock.nowMs(); calls++; }
    }
}
