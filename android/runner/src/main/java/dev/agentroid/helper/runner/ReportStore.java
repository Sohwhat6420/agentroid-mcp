package dev.agentroid.helper.runner;

import dev.agentroid.helper.core.Clock;
import dev.agentroid.helper.core.Json;

import java.util.ArrayList;
import java.util.LinkedHashMap;

/**
 * Where finished reports wait for the hub to pull them (request runner.report {segment_id}). It is the outbox's
 * Transport for the pull model: the link is "always up" for this purpose and send() only files the report, so a
 * report outlives a dropped link and a hub that asks twice gets the same answer.
 *
 * Bounded (wiring 2, item 3): a report is dropped when it is older than maxAgeMs, and the oldest go first when more
 * than maxCount are kept. The pruning runs on every write and every read, with the injected clock, so a long-lived
 * phone does not keep every report it ever made. A segment still running has no report yet, so nothing here can
 * drop the report of a running segment.
 *
 * Called from the runner's Clock thread (send) and from the link's thread (get): all methods are synchronized.
 */
public final class ReportStore implements ReportOutbox.Transport {
    public static final int MAX_REPORTS = 64;
    public static final long MAX_AGE_MS = 24L * 3600_000L;

    private static final class Entry {
        final String json;
        final long at;
        Entry(String json, long at) { this.json = json; this.at = at; }
    }

    private final Clock clock;
    private final int maxCount;
    private final long maxAgeMs;
    private final LinkedHashMap<String, Entry> byId = new LinkedHashMap<String, Entry>();
    private int unfiled;

    public ReportStore(Clock clock) { this(clock, MAX_REPORTS, MAX_AGE_MS); }

    public ReportStore(Clock clock, int maxCount, long maxAgeMs) {
        this.clock = clock;
        this.maxCount = maxCount;
        this.maxAgeMs = maxAgeMs;
    }

    @Override public boolean isUp() { return true; }

    /**
     * Files the report. A report that cannot be filed (not JSON, or no segment_id) is NOT acknowledged: it throws
     * IllegalArgumentException, which ReportOutbox counts as rejected and drops (returning false would hold every
     * later report behind it for ever, and returning true would claim a delivery that never happened).
     */
    @Override
    public synchronized boolean send(String reportJson) {
        String id = null;
        try {
            id = Segment.idOf(Json.parse(reportJson)); // a report carries segment_id at its top level, like a segment
        } catch (Json.ParseException e) {
            id = null;
        }
        if (id == null) {
            unfiled++;
            throw new IllegalArgumentException("a report with no readable segment_id cannot be filed");
        }
        byId.remove(id);
        byId.put(id, new Entry(reportJson, clock.nowMs()));
        prune();
        return true;
    }

    /** How many reports were refused because they could not be filed. */
    public synchronized int unfiled() { return unfiled; }

    /** The report text for a segment, or null when there is none (still running, unknown, or pruned). */
    public synchronized String get(String segmentId) {
        prune();
        Entry e = byId.get(segmentId);
        return e == null ? null : e.json;
    }

    public synchronized int size() { return byId.size(); }

    /** Drop the too-old reports, then the oldest beyond maxCount. Returns how many went. */
    public synchronized int prune() {
        int n = 0;
        long now = clock.nowMs();
        for (String id : new ArrayList<String>(byId.keySet())) {
            if (now - byId.get(id).at > maxAgeMs) { byId.remove(id); n++; }
        }
        while (byId.size() > maxCount) {
            byId.remove(byId.keySet().iterator().next());
            n++;
        }
        return n;
    }
}
