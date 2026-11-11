package dev.agentroid.helper.runner;

/**
 * What SegmentRunner.submit decided, at once: the segment was taken and is running (its report follows later), or it
 * was refused and nothing ran. The link's runner.segment answer is built from this; the report of a refusal is
 * queued as well, and the hub can still pull it with runner.report.
 */
public final class Admission {
    public final boolean accepted;
    /** "started", "refused" or "busy". */
    public final String outcome;
    /** The refusal code (a SegmentRefused code, step_not_safe, classification_too_low, busy), null when accepted. */
    public final String code;
    public final String reason;
    /** The segment id when it could be read, else null. */
    public final String segmentId;

    private Admission(boolean accepted, String outcome, String code, String reason, String segmentId) {
        this.accepted = accepted;
        this.outcome = outcome;
        this.code = code;
        this.reason = reason;
        this.segmentId = segmentId;
    }

    static Admission started(String segmentId) { return new Admission(true, "started", null, null, segmentId); }

    static Admission refused(String outcome, String code, String reason, String segmentId) {
        return new Admission(false, outcome, code, reason, segmentId);
    }
}
