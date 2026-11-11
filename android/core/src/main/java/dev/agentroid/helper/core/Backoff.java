package dev.agentroid.helper.core;

import java.util.Random;

/** Reconnect delay: exponential from 1 s, capped at 30 s (ADR 0001), with jitter so phones do not redial in step. */
public final class Backoff {
    private Backoff() { }

    public static final long MAX_MS = 30000;
    public static final long BASE_MS = 1000;

    /** Between half the cap for this attempt and the cap itself, so never above 30 s and never below 500 ms. */
    public static long delayMs(int attempt, Random rnd) {
        int shift = Math.min(Math.max(attempt, 0), 10);
        long cap = Math.min(MAX_MS, BASE_MS << shift);
        long half = cap / 2;
        return half + (long) (rnd.nextDouble() * (cap - half));
    }
}
