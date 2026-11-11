package dev.agentroid.helper.core;

import java.util.LinkedHashMap;
import java.util.Map;

/**
 * Per-link auth state. A new connection starts PENDING_AUTH; the hub's challenge moves it to READY or, on failure,
 * to NEEDS_REPAIR. NEEDS_REPAIR is sticky across disconnects: only a later challenge that passes clears it.
 * Only STATUS and AUTH methods are answered outside READY (see {@link Dispatcher}).
 */
public final class LinkSession {
    public enum State {
        PENDING_AUTH("pending_auth"), READY("ready"), NEEDS_REPAIR("needs_repair");

        private final String wire;
        State(String wire) { this.wire = wire; }
        public String wire() { return wire; }
    }

    /**
     * Default fix, used when no link-layer reason is known (the 2.5 RPC-level challenge path). Task 2.12 adds a fix per
     * reason ({@link #linkNeedsRepair}); the texts are fixed in schemas/link.json and mirrored in link.net.LinkProto.
     */
    public static final String REPAIR_FIX =
            "On the laptop, with the phone reachable over adb, run: agentroid link pair <serial>";

    private final Challenge challenge;
    private State state = State.PENDING_AUTH;
    private String failure;
    private String fix = REPAIR_FIX;
    private boolean notificationVisible = true;

    public LinkSession(Challenge challenge) { this.challenge = challenge; }

    public Challenge challenge() { return challenge; }

    public synchronized State state() { return state; }

    public synchronized String failure() { return failure; }

    public synchronized boolean notificationVisible() { return notificationVisible; }

    /** The service reports whether its foreground notification can actually be seen. No visible notification, no acting. */
    public synchronized void setNotificationVisible(boolean visible) { this.notificationVisible = visible; }

    public Challenge.Outcome authenticate(String nonceHex) {
        Challenge.Outcome o;
        try {
            o = challenge.answer(nonceHex);
        } catch (RuntimeException e) {
            o = Challenge.Outcome.fail("challenge raised " + e.getClass().getSimpleName());
        }
        if (o == null) o = Challenge.Outcome.fail("challenge returned nothing");
        synchronized (this) {
            if (o.ok) {
                state = State.READY;
                failure = null;
                fix = REPAIR_FIX;
            } else {
                state = State.NEEDS_REPAIR;
                failure = o.reason;
                fix = REPAIR_FIX;
            }
        }
        return o;
    }

    /** 2.12: the link-layer challenge passed and the hub sent ready. Clears a sticky needs_repair. */
    public synchronized void linkReady() {
        state = State.READY;
        failure = null;
        fix = REPAIR_FIX;
    }

    /** 2.12: the link-layer challenge failed (either side). reason is a link.json repair_reason; fix its exact text. */
    public synchronized void linkNeedsRepair(String reason, String fixText) {
        state = State.NEEDS_REPAIR;
        failure = reason;
        fix = fixText != null ? fixText : REPAIR_FIX;
    }

    /** The fix text for the current needs_repair (REPAIR_FIX when no link-layer reason is known). */
    public synchronized String fix() { return fix; }

    public synchronized void onDisconnect() {
        if (state == State.READY) state = State.PENDING_AUTH;
    }

    /** null when acting is allowed; otherwise the refusal to send. */
    public synchronized HandlerException gate() {
        if (state == State.NEEDS_REPAIR) {
            return new HandlerException(ErrorCodes.NEEDS_REPAIR, "needs_repair: request refused", repairData());
        }
        if (state == State.PENDING_AUTH) {
            return new HandlerException(ErrorCodes.AUTH_REQUIRED, "auth_required: send auth.challenge first", null);
        }
        if (!notificationVisible) {
            return new HandlerException(ErrorCodes.NOTIFICATION_HIDDEN,
                    "notification_hidden: the helper's notification cannot be shown, so it will not act", null);
        }
        return null;
    }

    private Map<String, Object> repairData() {
        Map<String, Object> d = new LinkedHashMap<String, Object>();
        d.put("state", "needs_repair");
        d.put("fix", fix);
        if (failure != null) d.put("reason", failure);
        return d;
    }
}
