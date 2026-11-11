package dev.agentroid.keeper;

import android.app.Activity;
import android.os.Bundle;

/**
 * Launcher entry point: the "app launch" trigger. Starts the keeper service and finishes immediately,
 * so tapping the icon (or an adb am start at setup) brings the service up without showing UI.
 */
public class MainActivity extends Activity {
    @Override
    protected void onCreate(Bundle b) {
        super.onCreate(b);
        L.init(this);
        L.i("MainActivity launch");
        // KeeperService is not exported, so `adb shell am start-foreground-service` is refused; setup goes through
        // this activity instead and its --es hub / --es serial extras are handed on to the service.
        android.content.Intent in = getIntent();
        String hub = in == null ? null : in.getStringExtra("hub");
        String serial = in == null ? null : in.getStringExtra("serial");
        String src = in == null ? null : in.getStringExtra("src");
        String pause = in == null ? null : in.getStringExtra("pause");
        android.content.Intent svc = new android.content.Intent(this, KeeperService.class)
                .putExtra("src", src == null ? "launch" : src);
        if (hub != null) svc.putExtra("hub", hub);
        if (serial != null) svc.putExtra("serial", serial);
        if (pause != null) svc.putExtra("pause", pause);
        try {
            startForegroundService(svc);
        } catch (Throwable t) {
            L.e("startForegroundService(launch) REFUSED", t);
        }
        finish();
    }
}
