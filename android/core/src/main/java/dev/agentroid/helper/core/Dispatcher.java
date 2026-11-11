package dev.agentroid.helper.core;

import java.util.HashMap;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.Map;

/**
 * Routes decoded requests to handlers and enforces the auth gate.
 *
 * <p>Every method is registered with a {@link Kind}; there is no default, so a new method cannot be added without
 * saying whether it acts. STATUS and AUTH answer in any link state. OBSERVE (reads the screen) and ACT (changes the
 * phone) are refused unless the session is READY, so after a failed challenge only status and a fresh challenge work.
 * ACT results are remembered by request id (ADR 0001: request ids are deduplicated), so a resent ACT request is
 * answered from memory and not performed twice.
 */
public final class Dispatcher {
    public enum Kind { STATUS, AUTH, OBSERVE, ACT }

    public interface Handler {
        Object handle(Map<String, Object> params) throws HandlerException;
    }

    private static final int DEDUPE_LIMIT = 256;

    private static final class Entry {
        final Kind kind;
        final Handler handler;
        Entry(Kind kind, Handler handler) { this.kind = kind; this.handler = handler; }
    }

    private static final class Done {
        final String method;
        final String paramsJson;
        final Map<String, Object> response;
        Done(String method, String paramsJson, Map<String, Object> response) {
            this.method = method;
            this.paramsJson = paramsJson;
            this.response = response;
        }
    }

    private final LinkSession session;
    private final Map<String, Entry> methods = new HashMap<String, Entry>();
    private final Map<String, Done> done = new LinkedHashMap<String, Done>();

    public Dispatcher(LinkSession session) { this.session = session; }

    public LinkSession session() { return session; }

    public void register(String method, Kind kind, Handler handler) {
        if (method == null || kind == null || handler == null) throw new IllegalArgumentException("method, kind and handler are required");
        if (methods.containsKey(method)) throw new IllegalStateException("method already registered: " + method);
        methods.put(method, new Entry(kind, handler));
    }

    /** Bytes in, bytes out. Never throws: every failure becomes an error response. */
    public byte[] handleFrame(byte[] frame) {
        Frames.Request r;
        try {
            r = Frames.decodeRequest(frame);
        } catch (FrameException e) {
            return Frames.toBytes(Frames.error(e.id, e.code, e.getMessage(), null));
        }
        return Frames.toBytes(dispatch(r));
    }

    public Map<String, Object> dispatch(Frames.Request r) {
        Entry e = methods.get(r.method);
        if (e == null) {
            return Frames.error(r.id, ErrorCodes.METHOD_NOT_FOUND, "unknown method '" + r.method + "'", null);
        }
        if (e.kind == Kind.OBSERVE || e.kind == Kind.ACT) {
            HandlerException g = session.gate();
            if (g != null) return Frames.error(r.id, g.code, g.getMessage(), g.data);
        }
        String key = null;
        String paramsJson = null;
        if (e.kind == Kind.ACT) {
            key = (r.id instanceof Long ? "n:" : "s:") + r.id;
            paramsJson = Json.write(r.params);
            synchronized (done) {
                Done d = done.get(key);
                if (d != null) {
                    if (d.method.equals(r.method) && d.paramsJson.equals(paramsJson)) return d.response;
                    return Frames.error(r.id, ErrorCodes.INVALID_REQUEST, "id already used by a different request", null);
                }
            }
        }
        Map<String, Object> response;
        boolean remember = true;
        try {
            response = Frames.result(r.id, e.handler.handle(r.params));
        } catch (HandlerException he) {
            response = Frames.error(r.id, he.code, he.getMessage(), he.data);
            // a transient answer ("busy", "not ready") is not remembered: the same id may be tried again
            if (he.code == ErrorCodes.TRANSIENT) remember = false;
        } catch (RuntimeException re) {
            // Class name only: exception text could carry typed text or screen content.
            response = Frames.error(r.id, ErrorCodes.INTERNAL_ERROR, "internal error: " + re.getClass().getSimpleName(), null);
        }
        if (key != null && remember) {
            synchronized (done) {
                done.put(key, new Done(r.method, paramsJson, response));
                if (done.size() > DEDUPE_LIMIT) {
                    Iterator<String> it = done.keySet().iterator();
                    it.next();
                    it.remove();
                }
            }
        }
        return response;
    }
}
