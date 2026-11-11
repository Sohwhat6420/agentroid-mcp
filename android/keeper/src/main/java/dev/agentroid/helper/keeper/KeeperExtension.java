package dev.agentroid.helper.keeper;

import android.content.ContentResolver;
import android.content.Context;
import android.database.ContentObserver;
import android.os.Handler;
import android.os.Looper;
import android.provider.Settings;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

import dev.agentroid.helper.core.Dispatcher;
import dev.agentroid.helper.core.ErrorCodes;
import dev.agentroid.helper.core.HandlerException;
import dev.agentroid.helper.keeper.logic.Bssids;
import dev.agentroid.helper.keeper.logic.Keeper;
import dev.agentroid.helper.link.LinkExtension;
import dev.agentroid.helper.link.net.LinkProto;

/**
 * The keeper riding on the link service (full flavour only; LinkService finds this class by name). It runs the
 * {@link Keeper} with the Android seams, wires the ContentObservers (on change), the Wi-Fi callback (on roam), the
 * 15-minute Handler tick plus an inexact AlarmManager backstop, and NSD discovery of the adb port; it reports
 * untrusted_bssid as a link event and merges its state into the heartbeat. It adds two link methods:
 * <pre>
 *   keeper.trust_bssids {bssids: ["aa:bb:cc:dd:ee:01", ...]}   ACT    replace the trusted list (one tap per mesh AP)
 *   keeper.status       {}                                    STATUS the beacon fields and the list size
 * </pre>
 */
public final class KeeperExtension implements LinkExtension {
    static volatile Keeper live;

    private Context ctx;
    private Keeper keeper;
    private AndroidKeeperParts.AndroidWifi wifi;
    private AndroidKeeperParts.TrustPrefs trust;
    private NsdAdbDiscovery nsd;
    private ContentObserver observer;

    public KeeperExtension() { }

    @Override
    public void attach(Context c, Dispatcher d, final Host host) {
        ctx = c.getApplicationContext();
        wifi = new AndroidKeeperParts.AndroidWifi(ctx);
        trust = new AndroidKeeperParts.TrustPrefs(ctx);
        keeper = new Keeper(new AndroidKeeperParts.HandlerClock(), new AndroidKeeperParts.AndroidSettings(ctx), wifi, trust,
                new Keeper.Reporter() {
                    public void untrustedBssid(String bssid, String reason) {
                        host.event(LinkProto.untrustedBssid(bssid, reason, System.currentTimeMillis()));
                    }
                }, AndroidKeeperParts.a11yComponent(ctx));
        final Keeper k = keeper;

        d.register("keeper.trust_bssids", Dispatcher.Kind.ACT, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> p) throws HandlerException {
                for (String key : p.keySet()) if (!key.equals("bssids")) throw new HandlerException(ErrorCodes.INVALID_PARAMS, "unknown param " + key);
                Object v = p.get("bssids");
                if (!(v instanceof List)) throw new HandlerException(ErrorCodes.INVALID_PARAMS, "bssids must be an array of strings");
                List<String> list = new ArrayList<String>();
                for (Object o : (List<?>) v) {
                    if (!(o instanceof String)) throw new HandlerException(ErrorCodes.INVALID_PARAMS, "bssids must be an array of strings");
                    list.add((String) o);
                }
                try {
                    Keeper.Result r = k.setTrusted(list);
                    Map<String, Object> out = new LinkedHashMap<String, Object>();
                    out.put("trusted", Long.valueOf(trust.get().size()));
                    out.put("keeper", r.state);
                    return out;
                } catch (Bssids.Invalid e) {
                    throw new HandlerException(ErrorCodes.INVALID_PARAMS, e.getMessage());
                }
            }
        });
        d.register("keeper.status", Dispatcher.Kind.STATUS, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> p) throws HandlerException {
                if (!p.isEmpty()) throw new HandlerException(ErrorCodes.INVALID_PARAMS, "keeper.status takes no params");
                Map<String, Object> out = new LinkedHashMap<String, Object>(k.beacon());
                out.put("trusted_count", Long.valueOf(trust.get().size()));
                return out;
            }
        });

        observer = new ContentObserver(new Handler(Looper.getMainLooper())) {
            @Override public void onChange(boolean selfChange) { k.onSettingChanged(); }
        };
        ContentResolver cr = ctx.getContentResolver();
        cr.registerContentObserver(Settings.Global.getUriFor(Keeper.ADB_WIFI), false, observer);
        cr.registerContentObserver(Settings.Secure.getUriFor(Keeper.A11Y_LIST), false, observer);
        cr.registerContentObserver(Settings.Secure.getUriFor(Keeper.A11Y_ON), false, observer);
        wifi.watch(new Runnable() { public void run() { k.onNetworkChanged(); } });
        keeper.start();
        nsd = new NsdAdbDiscovery(ctx, host.serial(), new NsdAdbDiscovery.Sink() {
            public void port(Integer port) { k.setAdbPort(port); }
        });
        nsd.start();
        KeeperTickReceiver.schedule(ctx);
        live = keeper;
    }

    @Override
    public Map<String, Object> beacon() {
        Keeper k = keeper;
        return k == null ? new LinkedHashMap<String, Object>() : k.beacon();
    }

    @Override
    public void detach() {
        live = null;
        if (keeper != null) keeper.stop();
        if (nsd != null) nsd.stop();
        if (wifi != null) wifi.stop();
        if (observer != null) ctx.getContentResolver().unregisterContentObserver(observer);
    }
}
