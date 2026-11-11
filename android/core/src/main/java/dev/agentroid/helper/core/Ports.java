package dev.agentroid.helper.core;

import java.util.List;
import java.util.Map;

/** What the Android modules implement, so the dispatcher can be tested without a phone. */
public final class Ports {
    private Ports() { }

    /** The phone side could not do a valid request. reason is a short machine word (e.g. secure_screen). */
    public static final class PortException extends Exception {
        public final String reason;
        public PortException(String reason, String message) {
            super(message);
            this.reason = reason;
        }
    }

    public static final class Screenshot {
        public final int width;
        public final int height;
        public final byte[] png;
        public Screenshot(int width, int height, byte[] png) {
            this.width = width;
            this.height = height;
            this.png = png;
        }
    }

    /** Implemented by the accessibility service (:a11y). Methods block until done and may be called off the main thread. */
    public interface A11y {
        List<? extends UiWindow> windows() throws PortException;
        void tap(double x, double y) throws PortException;
        void swipe(double x1, double y1, double x2, double y2, long durationMs) throws PortException;
        /** by is "id", "text" or "desc". One call: find the node, ACTION_SET_PROGRESS, read the value back. */
        Map<String, Object> setValue(String by, String selector, double value) throws PortException;
        Screenshot screenshot() throws PortException;
    }

    /** Implemented by the input method (:ime). */
    public interface Ime {
        /** Commit exactly this text at the cursor of the focused field. */
        void commitText(String text) throws PortException;
    }
}
