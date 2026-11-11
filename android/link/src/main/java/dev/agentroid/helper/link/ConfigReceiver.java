package dev.agentroid.helper.link;

import android.app.Activity;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

import dev.agentroid.helper.core.HubSpec;
import dev.agentroid.helper.link.net.LinkCrypto;
import dev.agentroid.helper.link.net.Pairing;
import dev.agentroid.helper.link.net.TailnetGuard;

/**
 * Laptop-side setup over adb (the manifest explains why this is exported and gated by android.permission.DUMP):
 *   adb shell am broadcast -n dev.agentroid.helper/.link.ConfigReceiver -a dev.agentroid.helper.PAIR \
 *       --es hub_key BASE64_SPKI --es hub 100.64.1.2:8765 --es serial SERIAL [--es laptop "Alex-PC"] [--ez rekey true]
 *   adb shell am broadcast -n dev.agentroid.helper/.link.ConfigReceiver -a dev.agentroid.helper.CONFIGURE --es hub 100.64.1.2:8765 --es laptop "Alex-PC"
 *   adb shell am broadcast -n dev.agentroid.helper/.link.ConfigReceiver -a dev.agentroid.helper.STOP
 * 2.12 PAIR carries the hub's PUBLIC key (a private key is refused, see Pairing), its tailnet address and the serial the
 * hub read; the reply names the phone's key id (public) so the hub can check it read the right dump. With rekey=true
 * the phone first replaces its own link key. Since 2.12 every address must be in 100.64.0.0/10 (or *.ts.net): loopback
 * is refused here too. Nothing secret travels; neither the address nor the name is logged.
 */
public class ConfigReceiver extends BroadcastReceiver {
    public static final String ACTION_CONFIGURE = "dev.agentroid.helper.CONFIGURE";
    public static final String ACTION_STOP = "dev.agentroid.helper.STOP";
    public static final String ACTION_PAIR = "dev.agentroid.helper.PAIR";

    @Override
    public void onReceive(Context c, Intent intent) {
        String action = intent.getAction();
        HubStore store = new HubStore(c);
        if (ACTION_STOP.equals(action)) {
            store.setEnabled(false);
            LinkService.stop(c);
            reply(Activity.RESULT_OK, "stopped");
        } else if (ACTION_PAIR.equals(action)) {
            try {
                Pairing p = Pairing.parse(intent.getStringExtra("hub_key"), intent.getStringExtra("hub"),
                        intent.getStringExtra("serial"), intent.getStringExtra("laptop"));
                KeystoreIdentity id = new KeystoreIdentity();
                if (intent.getBooleanExtra("rekey", false)) id.rekey();
                store.savePairing(p);
                LinkService.start(c, "pair");
                reply(Activity.RESULT_OK, "paired key_id=" + LinkCrypto.keyId(id.publicKeySpki()));
            } catch (Pairing.Invalid e) {
                reply(Activity.RESULT_CANCELED, "refused: " + e.getMessage());
            } catch (java.security.GeneralSecurityException e) {
                reply(Activity.RESULT_CANCELED, "refused: key_unavailable");
            }
        } else if (ACTION_CONFIGURE.equals(action)) {
            try {
                HubSpec spec = HubSpec.parse(intent.getStringExtra("hub"), intent.getStringExtra("laptop"));
                TailnetGuard.checkHost(spec.host);
                store.save(spec, true);
                LinkService.start(c, "config");
                reply(Activity.RESULT_OK, "configured");
            } catch (HubSpec.Invalid | TailnetGuard.Refused e) {
                reply(Activity.RESULT_CANCELED, "refused: " + e.getMessage());
            }
        }
    }

    private void reply(int code, String msg) {
        if (isOrderedBroadcast()) setResult(code, msg, null);
    }
}
