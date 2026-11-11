package dev.agentroid.helper.core;

/**
 * The auth seam (ADR 0001: the hub sends a nonce, the phone signs it). 2.5 is link-agnostic and ships only
 * {@link StubChallenge}; 2.12 replaces it with the AndroidKeyStore EC P-256 / SHA256withECDSA implementation.
 * Implementations must be safe to call from any thread and must not throw for an ordinary failure.
 */
public interface Challenge {
    final class Outcome {
        public final boolean ok;
        /** Signature text for the hub; null for the stub. */
        public final String signature;
        /** Why it failed; null when ok. Shown to the hub, so no secrets. */
        public final String reason;

        private Outcome(boolean ok, String signature, String reason) {
            this.ok = ok;
            this.signature = signature;
            this.reason = reason;
        }

        public static Outcome pass(String signature) { return new Outcome(true, signature, null); }
        public static Outcome fail(String reason) { return new Outcome(false, null, reason); }
    }

    /** Answer the hub's nonce (lower-case hex). */
    Outcome answer(String nonceHex);

    /** "stub" now, "SHA256withECDSA" in 2.12. Sent to the hub so a stub pass can never be mistaken for real auth. */
    String algorithm();
}
