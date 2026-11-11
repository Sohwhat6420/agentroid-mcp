package dev.agentroid.helper.overlay.logic;

import java.util.HashMap;
import java.util.HashSet;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Set;

/**
 * Which input device ids are the phone's own touchscreen (Detector 1's rules, as far as InputDevice exposes them):
 * sources include SOURCE_TOUCHSCREEN; not virtual (InputDevice.isVirtual(), id below 1); not external
 * (InputDevice.isExternal(), API 29+); no uhid / uinput / scrcpy / virtual in the name; and present when the service
 * started. A device added while the service runs (scrcpy's UHID devices, any uinput injector) never qualifies: a
 * phone's touchscreen does not hot-plug. The bus id (0006 for uhid and uinput) is NOT exposed to apps, so the
 * name, external and hot-plug rules stand in for it.
 */
public final class TouchscreenRegistry {
    /** InputDevice.SOURCE_TOUCHSCREEN. */
    public static final int SOURCE_TOUCHSCREEN = 0x00001002;

    /** What InputManager tells the app about one device. */
    public static final class Device {
        public final int id;
        public final String name;
        public final int sources;
        public final boolean virtual;
        public final boolean external;

        public Device(int id, String name, int sources, boolean virtual, boolean external) {
            this.id = id;
            this.name = name == null ? "" : name;
            this.sources = sources;
            this.virtual = virtual;
            this.external = external;
        }
    }

    private final Map<Integer, String> excluded = new HashMap<Integer, String>();
    private final Set<Integer> qualifying = new HashSet<Integer>();

    /** Why a device is not the phone's touchscreen, or null when it is. */
    public static String excludeReason(Device d) {
        if (d.virtual || d.id <= 0) return "virtual device";
        String n = d.name.toLowerCase(Locale.ROOT);
        for (String needle : new String[] {"uhid", "uinput", "scrcpy", "virtual"}) {
            if (n.contains(needle)) return "name says " + needle;
        }
        if (d.external) return "external device";
        if ((d.sources & SOURCE_TOUCHSCREEN) != SOURCE_TOUCHSCREEN) return "not a touchscreen";
        return null;
    }

    /** The devices present when the service starts. */
    public synchronized void seed(List<Device> devices) {
        qualifying.clear();
        excluded.clear();
        for (Device d : devices) {
            String why = excludeReason(d);
            if (why == null) qualifying.add(d.id);
            else excluded.put(d.id, why);
        }
    }

    /** InputDeviceListener.onInputDeviceAdded: never qualifies, whatever it says about itself. */
    public synchronized void onAdded(Device d) {
        qualifying.remove(d.id);
        excluded.put(d.id, "added while running (hot-plug)");
    }

    /** InputDeviceListener.onInputDeviceRemoved. The id stays excluded: ids are not reused while running. */
    public synchronized void onRemoved(int id) {
        if (qualifying.remove(id)) excluded.put(id, "removed");
    }

    public synchronized boolean qualifies(int deviceId) {
        return qualifying.contains(deviceId);
    }

    public synchronized String reason(int deviceId) {
        String r = excluded.get(deviceId);
        return r != null ? r : (qualifying.contains(deviceId) ? null : "unknown device");
    }
}
