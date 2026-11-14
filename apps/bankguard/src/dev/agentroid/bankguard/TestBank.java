package dev.agentroid.bankguard;

import android.app.Activity;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.widget.TextView;

/**
 * A stand-in bank for tests, so no real bank app is needed: put "dev.agentroid.bankguard/dev.agentroid.bankguard.TestBank"
 * on the list, open it with `adb shell am start -n dev.agentroid.bankguard/.TestBank --el close_after_ms 15000`, and
 * it closes itself after that long (at most 60 s). adb is gone while it is open, so it must be able to leave on its own.
 * It is not on the default list, so on a phone that never names it, it is just a screen that closes.
 */
public final class TestBank extends Activity {
    @Override
    protected void onCreate(Bundle b) {
        super.onCreate(b);
        TextView t = new TextView(this);
        t.setText("Agentroid Bank Guard: test bank (closes by itself)");
        t.setPadding(48, 96, 48, 48);
        setContentView(t);
        long ms = Math.max(1_000, Math.min(60_000, getIntent().getLongExtra("close_after_ms", 15_000)));
        new Handler(Looper.getMainLooper()).postDelayed(new Runnable() { public void run() { finish(); } }, ms);
    }
}
