package dev.agentroid.helper.core;

/** JSON-RPC 2.0 error codes, plus the helper's own in the -32000..-32099 server range. */
public final class ErrorCodes {
    private ErrorCodes() { }

    public static final int PARSE_ERROR = -32700;
    public static final int INVALID_REQUEST = -32600;
    public static final int METHOD_NOT_FOUND = -32601;
    public static final int INVALID_PARAMS = -32602;
    public static final int INTERNAL_ERROR = -32603;

    /** The challenge failed: every non-status request is refused until a new challenge passes. */
    public static final int NEEDS_REPAIR = -32001;
    /** No challenge has passed on this connection yet. */
    public static final int AUTH_REQUIRED = -32002;
    /** The phone cannot show its "helper is active" notification, so it will not act. */
    public static final int NOTIFICATION_HIDDEN = -32003;
    /** The request was valid but the phone side could not do it; data.reason says why. */
    public static final int DEVICE_ERROR = -32010;
    /**
     * An answer that must not be remembered: neither the phone's request-id memory nor the link's response cache nor
     * the hub's keeps it, so it can never pin an id (a busy runner, a runner that did not answer in time, a report
     * that is not ready yet). data.reason says which; data.outcome_unknown is true when the work may still be going on.
     */
    public static final int TRANSIENT = -32011;
}
