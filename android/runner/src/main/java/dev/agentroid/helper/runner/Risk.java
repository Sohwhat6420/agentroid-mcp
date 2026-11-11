package dev.agentroid.helper.runner;

/** Risk classes, the same scale as the flow format and crates/segments (R0 lowest). */
public enum Risk {
    R0, R1, R2, R3, R4, R5;

    public static Risk parse(Object o) {
        if (o instanceof String) {
            for (Risk r : values()) if (r.name().equals(o)) return r;
        }
        return null;
    }

    public static Risk max(Risk a, Risk b) { return a.ordinal() >= b.ordinal() ? a : b; }
}
