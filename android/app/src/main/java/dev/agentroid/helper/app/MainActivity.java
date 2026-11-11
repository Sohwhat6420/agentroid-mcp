package dev.agentroid.helper.app;

import android.Manifest;
import android.app.Activity;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.os.Build;
import android.os.Bundle;
import android.provider.Settings;
import android.view.View;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.TextView;

import dev.agentroid.helper.core.ApiGate;
import dev.agentroid.helper.core.PortRegistry;

/**
 * Status and setup shortcuts only. It reads no extras and acts on nothing it is sent; pairing and control come over
 * the link. It asks for the notification permission, because without a visible notification the helper will not act.
 */
public class MainActivity extends Activity {
    private TextView status;

    @Override
    protected void onCreate(Bundle b) {
        super.onCreate(b);
        LinearLayout root = new LinearLayout(this);
        root.setOrientation(LinearLayout.VERTICAL);
        int pad = (int) (16 * getResources().getDisplayMetrics().density);
        root.setPadding(pad, pad, pad, pad);

        status = new TextView(this);
        status.setTextSize(16);
        root.addView(status);
        root.addView(button("Open accessibility settings", Settings.ACTION_ACCESSIBILITY_SETTINGS));
        root.addView(button("Open keyboard settings", Settings.ACTION_INPUT_METHOD_SETTINGS));
        setContentView(root);

        // API 34+ only: hide this screen from accessibility services that are not accessibility tools.
        ApiGate.applyDataSensitive(Build.VERSION.SDK_INT, new ApiGate.DataSensitiveSink() {
            @Override public void setDataSensitive(boolean sensitive) {
                findViewById(android.R.id.content).setAccessibilityDataSensitive(
                        sensitive ? View.ACCESSIBILITY_DATA_SENSITIVE_YES : View.ACCESSIBILITY_DATA_SENSITIVE_NO);
            }
        });

        if (Build.VERSION.SDK_INT >= 33
                && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED) {
            requestPermissions(new String[] {Manifest.permission.POST_NOTIFICATIONS}, 1);
        }
    }

    @Override
    protected void onResume() {
        super.onResume();
        boolean a11y = PortRegistry.shared().a11yBound();
        status.setText("Agentroid helper\n\n"
                + "Accessibility service: " + (a11y ? "on" : "off (turn it on in accessibility settings)") + "\n"
                + "While this phone is being controlled, a notification names the laptop and has a Stop button.");
    }

    private Button button(String label, final String action) {
        Button bt = new Button(this);
        bt.setText(label);
        bt.setOnClickListener(new View.OnClickListener() {
            @Override public void onClick(View v) { startActivity(new Intent(action)); }
        });
        return bt;
    }
}
