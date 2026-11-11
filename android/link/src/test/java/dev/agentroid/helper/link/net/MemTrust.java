package dev.agentroid.helper.link.net;

/** In-memory TrustStore (the app's is link.HubStore). */
final class MemTrust implements TrustStore {
    byte[] hubKey;
    String serial;
    long maxEpoch;

    MemTrust(byte[] hubKey, String serial) {
        this.hubKey = hubKey;
        this.serial = serial;
    }

    @Override public byte[] hubKeySpki() { return hubKey; }
    @Override public String serial() { return serial; }
    @Override public long maxEpoch() { return maxEpoch; }
    @Override public void setMaxEpoch(long epoch) { maxEpoch = epoch; }
}
