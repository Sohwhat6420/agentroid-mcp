package dev.agentroid.helper.core;

/** Passes by default; {@link #forceFail(boolean)} makes it fail so the needs_repair path can be exercised. NOT security. */
public final class StubChallenge implements Challenge {
    private volatile boolean fail;

    public void forceFail(boolean fail) { this.fail = fail; }

    @Override
    public Outcome answer(String nonceHex) {
        if (fail) return Outcome.fail("stub challenge forced to fail");
        return Outcome.pass(null);
    }

    @Override
    public String algorithm() { return "stub"; }
}
