package dev.agentroid.helper.core;

/** A handler's refusal or failure, becomes a JSON-RPC error. */
public final class HandlerException extends Exception {
    public final int code;
    public final Object data;

    public HandlerException(int code, String message, Object data) {
        super(message);
        this.code = code;
        this.data = data;
    }

    public HandlerException(int code, String message) { this(code, message, null); }
}
