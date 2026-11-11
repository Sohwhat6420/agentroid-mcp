package dev.agentroid.helper.link;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.os.BatteryManager;
import android.os.Build;
import android.os.PowerManager;

/**
 * Charging and screen state for the heartbeat period (15 s charging or screen on, 60 s otherwise), and the battery
 * level for the beacon. The four broadcasts are system broadcasts; RECEIVER_NOT_EXPORTED keeps other apps out.
 */
final class AndroidPower {
    private final Context c;
    private final BroadcastReceiver receiver;

    AndroidPower(Context ctx, final Runnable onChange) {
        c = ctx.getApplicationContext();
        receiver = new BroadcastReceiver() {
            @Override public void onReceive(Context x, Intent i) { onChange.run(); }
        };
        IntentFilter f = new IntentFilter();
        f.addAction(Intent.ACTION_POWER_CONNECTED);
        f.addAction(Intent.ACTION_POWER_DISCONNECTED);
        f.addAction(Intent.ACTION_SCREEN_ON);
        f.addAction(Intent.ACTION_SCREEN_OFF);
        if (Build.VERSION.SDK_INT >= 33) c.registerReceiver(receiver, f, Context.RECEIVER_NOT_EXPORTED);
        else c.registerReceiver(receiver, f);
    }

    boolean charging() {
        BatteryManager bm = c.getSystemService(BatteryManager.class);
        return bm != null && bm.isCharging();
    }

    boolean screenOn() {
        PowerManager pm = c.getSystemService(PowerManager.class);
        return pm != null && pm.isInteractive();
    }

    /** 0..100, or null when unknown. */
    Long batteryPct() {
        BatteryManager bm = c.getSystemService(BatteryManager.class);
        if (bm == null) return null;
        int v = bm.getIntProperty(BatteryManager.BATTERY_PROPERTY_CAPACITY);
        return v >= 0 && v <= 100 ? Long.valueOf(v) : null;
    }

    void close() {
        try {
            c.unregisterReceiver(receiver);
        } catch (RuntimeException ignored) {
            // already gone
        }
    }
}
