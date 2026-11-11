package dev.agentroid.helper.runner;

/** A segment the phone will not run. code uses the same strings as crates/segments Refusal::code. */
public final class SegmentRefused extends Exception {
    public final String code;
    /** The segment id when it could be read (for the report), else null. */
    public final String segmentId;

    public SegmentRefused(String code, String detail, String segmentId) {
        super(code + ": " + detail);
        this.code = code;
        this.segmentId = segmentId;
    }
}
