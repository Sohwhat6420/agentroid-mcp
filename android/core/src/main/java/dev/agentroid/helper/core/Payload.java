package dev.agentroid.helper.core;

import java.util.Base64;

/**
 * Binary-in-JSON transport encoding for screenshot PNGs, and nothing else. This is NOT protection: it hides nothing,
 * and no config, key or secret passes through it. The clean-room audit allows Base64 in this one file only.
 */
public final class Payload {
    private Payload() { }

    public static String b64(byte[] data) { return Base64.getEncoder().encodeToString(data); }
}
