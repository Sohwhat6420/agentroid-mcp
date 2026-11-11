package dev.agentroid.helper.core;

/** Facts only the Android side can read. Implemented in :link; faked in tests. */
public interface HelperEnv {
    String helperVersion();
    int sdkInt();
    boolean imeEnabled();
    boolean imeSelected();
}
