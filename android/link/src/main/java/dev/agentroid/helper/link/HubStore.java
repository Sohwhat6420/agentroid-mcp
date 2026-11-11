package dev.agentroid.helper.link;

import android.content.Context;
import android.content.SharedPreferences;

import dev.agentroid.helper.core.HubSpec;
import dev.agentroid.helper.link.net.LinkCrypto;
import dev.agentroid.helper.link.net.Pairing;
import dev.agentroid.helper.link.net.TrustStore;

/**
 * What the helper stores: the hub address, the laptop's display name, an on/off flag and, since 2.12, the hub's
 * PUBLIC key, the serial the hub read over adb, and the highest hub epoch that reached ready. None of it is a secret
 * (the phone's private key lives only in AndroidKeyStore). MODE_PRIVATE, so only this app's uid can read the file;
 * backup is off in the manifest. The address is validated again on every load.
 *
 * 2.12: the file lives in DEVICE-PROTECTED storage (moved there once from the old location), so the directBootAware
 * link can read it before the first unlock. The epoch is written with commit(), not apply(): fencing must survive a
 * crash right after a ready.
 */
final class HubStore implements TrustStore {
    private static final String FILE = "hub";
    private final SharedPreferences prefs;

    HubStore(Context c) {
        Context de = c.createDeviceProtectedStorageContext();
        de.moveSharedPreferencesFrom(c, FILE); // no-op once moved
        prefs = de.getSharedPreferences(FILE, Context.MODE_PRIVATE);
    }

    void save(HubSpec spec, boolean enabled) {
        prefs.edit().putString("hub", spec.host + ":" + spec.port).putString("laptop", spec.laptopLabel)
                .putBoolean("enabled", enabled).apply();
    }

    /** The DUMP-protected pairing broadcast: hub public key, tailnet address and serial, all checked by Pairing. */
    void savePairing(Pairing p) {
        prefs.edit().putString("hub", p.hub.host + ":" + p.hub.port).putString("laptop", p.hub.laptopLabel)
                .putString("hub_key", LinkCrypto.b64(p.hubKeySpki)).putString("serial", p.serial)
                .putBoolean("enabled", true).commit();
    }

    @Override
    public byte[] hubKeySpki() {
        byte[] der = LinkCrypto.unb64(prefs.getString("hub_key", null));
        try {
            if (der != null) LinkCrypto.p256(der);
            return der;
        } catch (java.security.GeneralSecurityException | RuntimeException e) {
            return null;
        }
    }

    @Override public String serial() { return prefs.getString("serial", null); }

    @Override public long maxEpoch() { return prefs.getLong("max_epoch", 0L); }

    @Override public void setMaxEpoch(long epoch) { prefs.edit().putLong("max_epoch", epoch).commit(); }

    void setEnabled(boolean enabled) { prefs.edit().putBoolean("enabled", enabled).apply(); }

    boolean enabled() { return prefs.getBoolean("enabled", false); }

    /** The stored spec, or null when none is stored or it no longer validates. */
    HubSpec load() {
        String hub = prefs.getString("hub", null);
        if (hub == null) return null;
        try {
            return HubSpec.parse(hub, prefs.getString("laptop", null));
        } catch (HubSpec.Invalid e) {
            return null;
        }
    }
}
