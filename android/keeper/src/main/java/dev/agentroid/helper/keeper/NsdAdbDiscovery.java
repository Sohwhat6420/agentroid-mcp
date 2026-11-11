package dev.agentroid.helper.keeper;

import android.content.Context;
import android.net.nsd.NsdManager;
import android.net.nsd.NsdServiceInfo;

import java.net.InetAddress;
import java.net.NetworkInterface;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

import dev.agentroid.helper.keeper.logic.AdbServices;

/**
 * Finds this phone's wireless-debugging port the way the laptop's adb does: mDNS `_adb-tls-connect._tcp` through
 * NsdManager. Each found service is resolved and passed to AdbServices.pick, which keeps only
 * this phone's own. Only discovery: it opens no socket and listens on nothing (the system's mDNS daemon does the work).
 */
final class NsdAdbDiscovery {
    interface Sink { void port(Integer port); }

    private final NsdManager nsd;
    private final Sink sink;
    private final String serial;
    private final List<AdbServices.Found> found = new ArrayList<AdbServices.Found>();
    private NsdManager.DiscoveryListener listener;

    NsdAdbDiscovery(Context c, String serial, Sink sink) {
        this.nsd = c.getSystemService(NsdManager.class);
        this.serial = serial;
        this.sink = sink;
    }

    void start() {
        listener = new NsdManager.DiscoveryListener() {
            @Override public void onStartDiscoveryFailed(String t, int e) { sink.port(null); }
            @Override public void onStopDiscoveryFailed(String t, int e) { }
            @Override public void onDiscoveryStarted(String t) { }
            @Override public void onDiscoveryStopped(String t) { }
            @Override public void onServiceFound(NsdServiceInfo s) { resolve(s); }
            @Override public void onServiceLost(NsdServiceInfo s) {
                synchronized (found) {
                    for (int i = found.size() - 1; i >= 0; i--) if (found.get(i).name.equals(s.getServiceName())) found.remove(i);
                }
                publish();
            }
        };
        try {
            nsd.discoverServices(AdbServices.SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, listener);
        } catch (RuntimeException e) {
            sink.port(null);
        }
    }

    @SuppressWarnings("deprecation") // resolveService is deprecated on API 34 but is the only call on API 29..33
    private void resolve(NsdServiceInfo s) {
        try {
            nsd.resolveService(s, new NsdManager.ResolveListener() {
                @Override public void onResolveFailed(NsdServiceInfo i, int e) { }
                @Override public void onServiceResolved(NsdServiceInfo i) {
                    InetAddress h = i.getHost();
                    synchronized (found) {
                        found.add(new AdbServices.Found(i.getServiceName(), h == null ? null : h.getHostAddress(), i.getPort()));
                    }
                    publish();
                }
            });
        } catch (RuntimeException ignored) {
            // a resolve already in flight; the next found event retries
        }
    }

    private void publish() {
        List<AdbServices.Found> copy;
        synchronized (found) {
            copy = new ArrayList<AdbServices.Found>(found);
        }
        sink.port(AdbServices.pick(copy, localAddresses(), serial));
    }

    private static List<String> localAddresses() {
        List<String> out = new ArrayList<String>();
        try {
            for (NetworkInterface ni : Collections.list(NetworkInterface.getNetworkInterfaces())) {
                for (InetAddress a : Collections.list(ni.getInetAddresses())) out.add(a.getHostAddress());
            }
        } catch (Exception ignored) {
            // no addresses: pick falls back to the serial in the service name
        }
        return out;
    }

    void stop() {
        if (listener == null) return;
        try {
            nsd.stopServiceDiscovery(listener);
        } catch (RuntimeException ignored) {
            // not running
        }
    }
}
