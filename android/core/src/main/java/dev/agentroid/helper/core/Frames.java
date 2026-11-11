package dev.agentroid.helper.core;

import java.nio.ByteBuffer;
import java.nio.charset.CharacterCodingException;
import java.nio.charset.CodingErrorAction;
import java.nio.charset.StandardCharsets;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.regex.Pattern;

/**
 * Link framing and message shapes.
 *
 * <p>ADR 0001 fixes the transport (one authenticated WebSocket the phone dials out) but not the message shape. This
 * class implements the shape the plan names for the daemon's own IPC:
 * <ul>
 *   <li>One WebSocket message (text, or binary holding UTF-8) = exactly one JSON-RPC 2.0 object. No batches.</li>
 *   <li>Request: {"jsonrpc":"2.0","id":&lt;string|integer&gt;,"method":"tap","params":{...},"seq":&lt;int, optional&gt;}.
 *       Anything else at the top level is refused. "id" is required: the helper always answers.</li>
 *   <li>Response: {"jsonrpc":"2.0","id":..,"result":..} or {"jsonrpc":"2.0","id":..,"error":{"code","message","data"}}.</li>
 *   <li>Helper-initiated events (hello, ...) are JSON-RPC notifications: no "id".</li>
 * </ul>
 * ASSUMED (flagged in the report): the exact method names and the optional "seq" resume field, since neither the ADR
 * nor the schemas folder defines them. Text is UTF-8 end to end; invalid UTF-8 is refused, never repaired.
 */
public final class Frames {
    private Frames() { }

    /** Requests are small. Anything larger is refused before parsing. Responses (tree, screenshot) are not limited here. */
    public static final int MAX_REQUEST_BYTES = 64 * 1024;
    private static final Pattern METHOD = Pattern.compile("[a-z][a-z0-9_]*(\\.[a-z][a-z0-9_]*)*");

    public static final class Request {
        public final Object id;
        public final String method;
        public final Map<String, Object> params;
        public final Long seq;

        Request(Object id, String method, Map<String, Object> params, Long seq) {
            this.id = id;
            this.method = method;
            this.params = params;
            this.seq = seq;
        }
    }

    // ------------------------------------------------------------------ decode

    /** Strict UTF-8: a malformed sequence is an error, never U+FFFD. */
    public static String decodeUtf8Strict(byte[] b) throws CharacterCodingException {
        return StandardCharsets.UTF_8.newDecoder()
                .onMalformedInput(CodingErrorAction.REPORT)
                .onUnmappableCharacter(CodingErrorAction.REPORT)
                .decode(ByteBuffer.wrap(b)).toString();
    }

    @SuppressWarnings("unchecked")
    public static Request decodeRequest(byte[] frame) throws FrameException {
        if (frame == null || frame.length == 0) {
            throw new FrameException(ErrorCodes.INVALID_REQUEST, "empty frame", null);
        }
        if (frame.length > MAX_REQUEST_BYTES) {
            throw new FrameException(ErrorCodes.INVALID_REQUEST, "frame larger than " + MAX_REQUEST_BYTES + " bytes", null);
        }
        String text;
        try {
            text = decodeUtf8Strict(frame);
        } catch (CharacterCodingException e) {
            throw new FrameException(ErrorCodes.PARSE_ERROR, "frame is not valid UTF-8", null);
        }
        Object root;
        try {
            root = Json.parse(text);
        } catch (Json.ParseException e) {
            throw new FrameException(ErrorCodes.PARSE_ERROR, "invalid JSON: " + e.getMessage(), null);
        }
        if (!(root instanceof Map)) {
            throw new FrameException(ErrorCodes.INVALID_REQUEST, "a frame must be one JSON object (no batches)", null);
        }
        Map<String, Object> m = (Map<String, Object>) root;

        // The id first, so every later refusal can be addressed to it.
        Object id = m.get("id");
        if (!(id instanceof Long) && !(id instanceof String)) {
            throw new FrameException(ErrorCodes.INVALID_REQUEST, "id must be a string or an integer", null);
        }
        if (id instanceof String && (((String) id).isEmpty() || ((String) id).length() > 128)) {
            throw new FrameException(ErrorCodes.INVALID_REQUEST, "id must be 1..128 characters", null);
        }
        for (String k : m.keySet()) {
            if (!k.equals("jsonrpc") && !k.equals("id") && !k.equals("method") && !k.equals("params") && !k.equals("seq")) {
                throw new FrameException(ErrorCodes.INVALID_REQUEST, "unknown top-level field '" + k + "'", id);
            }
        }
        if (!"2.0".equals(m.get("jsonrpc"))) {
            throw new FrameException(ErrorCodes.INVALID_REQUEST, "jsonrpc must be \"2.0\"", id);
        }
        Object method = m.get("method");
        if (!(method instanceof String) || ((String) method).length() > 64 || !METHOD.matcher((String) method).matches()) {
            throw new FrameException(ErrorCodes.INVALID_REQUEST, "method must be a dotted lower-case name of at most 64 characters", id);
        }
        Object params = m.get("params");
        Map<String, Object> pm;
        if (params == null && !m.containsKey("params")) {
            pm = new LinkedHashMap<String, Object>();
        } else if (params instanceof Map) {
            pm = (Map<String, Object>) params;
        } else {
            throw new FrameException(ErrorCodes.INVALID_PARAMS, "params must be an object", id);
        }
        Long seq = null;
        if (m.containsKey("seq")) {
            Object s = m.get("seq");
            if (!(s instanceof Long) || ((Long) s).longValue() < 0) {
                throw new FrameException(ErrorCodes.INVALID_REQUEST, "seq must be a non-negative integer", id);
            }
            seq = (Long) s;
        }
        return new Request(id, (String) method, pm, seq);
    }

    // ------------------------------------------------------------------ encode

    public static Map<String, Object> result(Object id, Object result) {
        Map<String, Object> m = new LinkedHashMap<String, Object>();
        m.put("jsonrpc", "2.0");
        m.put("id", id);
        m.put("result", result);
        return m;
    }

    public static Map<String, Object> error(Object id, int code, String message, Object data) {
        Map<String, Object> e = new LinkedHashMap<String, Object>();
        e.put("code", Long.valueOf(code));
        e.put("message", message);
        if (data != null) e.put("data", data);
        Map<String, Object> m = new LinkedHashMap<String, Object>();
        m.put("jsonrpc", "2.0");
        m.put("id", id);
        m.put("error", e);
        return m;
    }

    public static Map<String, Object> notification(String method, Map<String, Object> params) {
        Map<String, Object> m = new LinkedHashMap<String, Object>();
        m.put("jsonrpc", "2.0");
        m.put("method", method);
        m.put("params", params);
        return m;
    }

    public static byte[] toBytes(Map<String, Object> message) {
        return Json.write(message).getBytes(StandardCharsets.UTF_8);
    }
}
