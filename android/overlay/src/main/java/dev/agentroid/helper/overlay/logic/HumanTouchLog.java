package dev.agentroid.helper.overlay.logic;

/**
 * The phone's local {@code last_human_touch}. It works with adb down: every new touch is written to a {@link Store}
 * (SharedPreferences on the phone) so the 2.14 runner can honour the {@code human_touch} flag without the hub, and a
 * {@code {"t":"human_touch","ts":...}} frame is queued for the link. Touches within 50 ms of the last one merge (the
 * same finger seen twice); the latest timestamp wins.
 */
public final class HumanTouchLog {
    public static final long MERGE_MS = 50;

    /** Durable storage for the flag. */
    public interface Store {
        void putLastTouch(long wallMs);
        long lastTouch();
    }

    /** Where frames go (the link's outbox; it may be down). */
    public interface Sink {
        void send(String frame);
    }

    private final Store store;
    private final Sink sink;
    private long last = -1;
    private int merged, recorded;

    public HumanTouchLog(Store store, Sink sink) {
        this.store = store;
        this.sink = sink;
        long persisted = store.lastTouch();
        this.last = persisted > 0 ? persisted : -1;
    }

    /** Record a human touch at wall time {@code wallMs}. Returns false when it merged with the last one. */
    public synchronized boolean record(long wallMs) {
        if (last >= 0 && Math.abs(wallMs - last) <= MERGE_MS) {
            last = Math.max(last, wallMs);
            merged++;
            return false;
        }
        last = Math.max(last, wallMs);
        recorded++;
        store.putLastTouch(last);
        sink.send(frame(wallMs));
        return true;
    }

    public static String frame(long ts) {
        return "{\"t\":\"human_touch\",\"ts\":" + ts + "}";
    }

    public synchronized long last() { return last; }
    public synchronized int merged() { return merged; }
    public synchronized int recorded() { return recorded; }

    /** The local flag 2.14 reads: a human touched within {@code windowMs} of {@code nowMs}. */
    public synchronized boolean humanTouchWithin(long nowMs, long windowMs) {
        return last >= 0 && nowMs - last <= windowMs;
    }
}
