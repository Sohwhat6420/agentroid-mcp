package dev.agentroid.helper.link.net;

import java.security.SecureRandom;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.Random;

import dev.agentroid.helper.core.Dispatcher;
import dev.agentroid.helper.core.ErrorCodes;
import dev.agentroid.helper.core.HandlerException;
import dev.agentroid.helper.core.LinkSession;

/**
 * The phone side for the link tests: the real LinkClient, the real core Dispatcher and LinkSession, a software key,
 * and one ACT method, "act.count", that records how many times each request ran (by its param "n").
 */
final class Phone implements LinkEnv {
    final SoftwareIdentity key;
    final MemTrust trust;
    final LinkSession session;
    final Dispatcher dispatcher;
    final LinkClient client;
    final Map<String, Integer> runs = new HashMap<String, Integer>();
    boolean charging, screenOn;
    final Map<String, Object> beacon = new LinkedHashMap<String, Object>();
    int stateChanges;

    Phone(FakeClock clock, FakeHub hub, long keySeed) {
        key = new SoftwareIdentity(keySeed);
        trust = new MemTrust(hub.hubSpki(), hub.serial);
        final LinkClient[] self = new LinkClient[1];
        session = new LinkSession(new LinkChallenge(new LinkChallenge.ReadyCheck() {
            public boolean linkReady() { return self[0] != null && self[0].phase() == LinkClient.Phase.READY; }
        }));
        dispatcher = new Dispatcher(session);
        dispatcher.register("act.count", Dispatcher.Kind.ACT, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> p) throws HandlerException {
                String n = String.valueOf(p.get("n"));
                Integer c = runs.get(n);
                runs.put(n, c == null ? 1 : c + 1);
                Map<String, Object> r = new LinkedHashMap<String, Object>();
                r.put("n", p.get("n"));
                return r;
            }
        });
        // answers TRANSIENT the first time it is asked for an "n", and a result after that
        dispatcher.register("act.flaky", Dispatcher.Kind.ACT, new Dispatcher.Handler() {
            public Object handle(Map<String, Object> p) throws HandlerException {
                String n = String.valueOf(p.get("n"));
                Integer c = runs.get(n);
                int now = c == null ? 1 : c + 1;
                runs.put(n, now);
                if (now == 1) throw new HandlerException(ErrorCodes.TRANSIENT, "busy", null);
                Map<String, Object> r = new LinkedHashMap<String, Object>();
                r.put("n", p.get("n"));
                return r;
            }
        });
        client = new LinkClient(clock, hub, key, trust, session, this, new SecureRandom(new byte[] {2}), new Random(keySeed),
                "0.2.0-test", 34);
        self[0] = client;
        hub.phoneKey = key.publicKeySpki();
        beacon.put("battery_pct", Long.valueOf(80));
    }

    void start() {
        try {
            client.start("100.101.102.103", 8765);
        } catch (TailnetGuard.Refused e) {
            throw new AssertionError(e);
        }
    }

    int totalRuns() {
        int s = 0;
        for (int v : runs.values()) s += v;
        return s;
    }

    @Override public boolean charging() { return charging; }
    @Override public boolean screenOn() { return screenOn; }
    @Override public Map<String, Object> beacon() { return beacon; }
    @Override public byte[] execute(byte[] rpcFrame) { return dispatcher.handleFrame(rpcFrame); }
    @Override public void onStateChanged() { stateChanges++; }
}
