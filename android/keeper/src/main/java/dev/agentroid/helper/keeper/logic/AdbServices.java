package dev.agentroid.helper.keeper.logic;

import java.util.Collection;
import java.util.Locale;

/**
 * Picks THIS phone's wireless-debugging port out of what NsdManager finds for `_adb-tls-connect._tcp` (every phone on
 * the LAN with wireless debugging on advertises one). A service is ours when its resolved host is one of this
 * phone's own addresses, or, failing that, when its instance name is "adb-SERIAL-..." for the serial the hub paired.
 */
public final class AdbServices {
    private AdbServices() { }

    public static final String SERVICE_TYPE = "_adb-tls-connect._tcp";

    public static final class Found {
        public final String name;
        public final String hostAddress;
        public final int port;
        public Found(String name, String hostAddress, int port) {
            this.name = name;
            this.hostAddress = hostAddress;
            this.port = port;
        }
    }

    /** The port, or null when nothing found is ours. */
    public static Integer pick(Collection<Found> found, Collection<String> localAddresses, String serial) {
        if (found == null) return null;
        Integer byName = null;
        for (Found f : found) {
            if (f == null || f.port < 1 || f.port > 65535 || f.name == null || !f.name.startsWith("adb-")) continue;
            if (f.hostAddress != null && localAddresses != null && localAddresses.contains(f.hostAddress)) return f.port;
            if (serial != null && !serial.isEmpty()
                    && f.name.toLowerCase(Locale.ROOT).startsWith(("adb-" + serial + "-").toLowerCase(Locale.ROOT))) byName = f.port;
        }
        return byName;
    }
}
