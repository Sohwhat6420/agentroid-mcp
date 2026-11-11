package dev.agentroid.helper.runner;

/**
 * Gemini Computer Use's safety_decision values, as the 2.14 plan names them. In Phase 2 there is no approval channel,
 * so {@link #phaseTwo} turns REQUIRE_CONFIRMATION into BLOCKED (crates/segments Phase::Two does the same).
 */
public enum Decision {
    ALLOWED("allowed"), REQUIRE_CONFIRMATION("require_confirmation"), BLOCKED("blocked");

    public final String wire;

    Decision(String wire) { this.wire = wire; }

    /** Phase 2: nothing can ask the device owner, so a confirmation is a block. */
    public static Decision phaseTwo(Decision d) {
        return d == REQUIRE_CONFIRMATION ? BLOCKED : d;
    }
}
