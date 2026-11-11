package dev.agentroid.helper.core;

/**
 * Where the services publish themselves. The accessibility service and the IME register on connect and clear on
 * destroy; the link looks them up per request, so start order does not matter. {@link #shared()} is the one the
 * Android code uses; tests build their own.
 */
public final class PortRegistry {
    private static final PortRegistry SHARED = new PortRegistry();

    public static PortRegistry shared() { return SHARED; }

    private volatile Ports.A11y a11y;
    private volatile Ports.Ime ime;
    private final java.util.concurrent.CopyOnWriteArrayList<HumanTouchListener> humanTouch =
            new java.util.concurrent.CopyOnWriteArrayList<HumanTouchListener>();

    /**
     * Told of every human touch the overlay sees, at SystemClock.elapsedRealtime(). The app's runner flag listens, and
     * so does the link (a human_touch frame for the hub's take-over machine).
     */
    public interface HumanTouchListener {
        void touched(long elapsedRealtimeMs);
    }

    public void addHumanTouchListener(HumanTouchListener l) { if (l != null) humanTouch.addIfAbsent(l); }

    public void removeHumanTouchListener(HumanTouchListener l) { humanTouch.remove(l); }

    /** From the overlay (main thread). One listener that throws does not stop the others. */
    public void humanTouched(long elapsedRealtimeMs) {
        for (HumanTouchListener l : humanTouch) {
            try {
                l.touched(elapsedRealtimeMs);
            } catch (RuntimeException e) {
                // a broken listener must not hide the touch from the rest
            }
        }
    }

    public void setA11y(Ports.A11y p) { a11y = p; }
    public void setIme(Ports.Ime p) { ime = p; }

    /** Clear only if p is still the registered one (a newer instance may already have replaced it). */
    public void clearA11y(Ports.A11y p) { if (a11y == p) a11y = null; }
    public void clearIme(Ports.Ime p) { if (ime == p) ime = null; }

    public boolean a11yBound() { return a11y != null; }
    public boolean imeLive() { return ime != null; }

    public Ports.A11y a11y() throws Ports.PortException {
        Ports.A11y p = a11y;
        if (p == null) throw new Ports.PortException("a11y_unbound", "the accessibility service is not bound");
        return p;
    }

    public Ports.Ime ime() throws Ports.PortException {
        Ports.Ime p = ime;
        if (p == null) throw new Ports.PortException("ime_not_active", "the Agentroid input method is not the active keyboard");
        return p;
    }
}
