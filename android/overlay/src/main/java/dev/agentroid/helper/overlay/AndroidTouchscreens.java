package dev.agentroid.helper.overlay;

import android.content.Context;
import android.hardware.input.InputManager;
import android.os.Handler;
import android.view.InputDevice;

import java.util.ArrayList;
import java.util.List;

import dev.agentroid.helper.overlay.logic.TouchscreenRegistry;

/** Thin glue: fills a {@link TouchscreenRegistry} from InputManager and keeps it current (hot-plugs never qualify). */
final class AndroidTouchscreens implements InputManager.InputDeviceListener {
    private final InputManager im;
    final TouchscreenRegistry registry = new TouchscreenRegistry();

    AndroidTouchscreens(Context context) {
        im = context.getSystemService(InputManager.class);
    }

    private static TouchscreenRegistry.Device describe(InputDevice d) {
        return new TouchscreenRegistry.Device(d.getId(), d.getName(), d.getSources(), d.isVirtual(), d.isExternal());
    }

    void start(Handler handler) {
        List<TouchscreenRegistry.Device> now = new ArrayList<TouchscreenRegistry.Device>();
        for (int id : im.getInputDeviceIds()) {
            InputDevice d = im.getInputDevice(id);
            if (d != null) now.add(describe(d));
        }
        registry.seed(now);
        im.registerInputDeviceListener(this, handler);
    }

    void stop() {
        im.unregisterInputDeviceListener(this);
    }

    @Override public void onInputDeviceAdded(int deviceId) {
        InputDevice d = im.getInputDevice(deviceId);
        registry.onAdded(d != null ? describe(d) : new TouchscreenRegistry.Device(deviceId, "", 0, false, false));
    }

    @Override public void onInputDeviceRemoved(int deviceId) {
        registry.onRemoved(deviceId);
    }

    @Override public void onInputDeviceChanged(int deviceId) { }
}
