package dev.agentroid.helper.core;

/** A frame that cannot become a request. Carries the id if one could be read, so the refusal can be answered. */
public final class FrameException extends Exception {
    public final int code;
    /** String, Long, or null when the id was missing or unreadable. */
    public final Object id;

    public FrameException(int code, String message, Object id) {
        super(message);
        this.code = code;
        this.id = id;
    }
}
