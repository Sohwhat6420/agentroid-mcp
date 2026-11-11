package dev.agentroid.helper.link.net;

/**
 * What the phone remembers about its hub. None of it is secret: the hub's PUBLIC key (from the DUMP-protected pairing
 * broadcast), the serial the hub read over adb, and the highest epoch that reached ready (epoch fencing survives a
 * restart). In the app this is link.HubStore (device-protected, MODE_PRIVATE prefs); the tests use memory.
 */
public interface TrustStore {
    /** The hub's X.509 SPKI DER, or null when the phone was never paired. */
    byte[] hubKeySpki();

    /** ro.serialno as the hub read it at pairing, or null. */
    String serial();

    long maxEpoch();

    void setMaxEpoch(long epoch);
}
