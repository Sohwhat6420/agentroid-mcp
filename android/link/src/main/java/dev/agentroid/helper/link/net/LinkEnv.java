package dev.agentroid.helper.link.net;

import java.util.Map;

/** Phone facts the link needs: power state (heartbeat period), beacon fields, and how a request is run. */
public interface LinkEnv {
    boolean charging();

    boolean screenOn();

    /**
     * Beacon fields merged into each heartbeat (link.json $defs/heartbeat): battery_pct, adb_port, adb_port_source,
     * wifi_adb, a11y_enabled, keeper, bssid_trusted. Missing keys are filled with the "absent" values.
     */
    Map<String, Object> beacon();

    /** Run one JSON-RPC request frame and return the response frame (core Dispatcher.handleFrame). */
    byte[] execute(byte[] rpcFrame);

    /** Link state changed (for the notification). Called on the link thread. */
    void onStateChanged();
}
