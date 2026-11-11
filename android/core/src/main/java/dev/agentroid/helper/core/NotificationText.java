package dev.agentroid.helper.core;

/** Wording of the always-visible notification. It must name the controlling laptop and say plainly what state we are in. */
public final class NotificationText {
    private NotificationText() { }

    public static final String TITLE = "Agentroid helper is active";

    public static String body(LinkSession.State state, boolean connected, String laptop) {
        if (state == LinkSession.State.NEEDS_REPAIR) {
            return "Needs re-pairing with " + laptop + ". All control requests are refused.";
        }
        if (!connected) {
            return "Waiting to connect to " + laptop + ". Nothing can control this phone until it connects.";
        }
        if (state == LinkSession.State.READY) {
            return "This phone is controlled by " + laptop + ".";
        }
        return "Connected to " + laptop + ", checking its identity.";
    }
}
