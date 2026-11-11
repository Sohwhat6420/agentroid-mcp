package dev.agentroid.helper.link.net;

import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.security.SecureRandom;
import java.util.ArrayList;
import java.util.Iterator;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Random;
import java.util.TreeMap;

import dev.agentroid.helper.core.Backoff;
import dev.agentroid.helper.core.Clock;
import dev.agentroid.helper.core.ErrorCodes;
import dev.agentroid.helper.core.Frames;
import dev.agentroid.helper.core.HubSpec;
import dev.agentroid.helper.core.Json;
import dev.agentroid.helper.core.LinkSession;

/**
 * The persistent link, phone side (task 2.12, protocol v1 in schemas/link.json). It DIALS OUT through a
 * {@link Transport}; it never listens.
 *
 * <ul>
 *   <li>Handshake: hello (with a fresh phone nonce) -> the hub's challenge, whose signature by the paired hub key must
 *       cover our nonce, the epoch, our serial and our key id -> our SHA256withECDSA response -> the hub's ready.
 *       Any failure goes to needs_repair with the exact fix text for its reason, and the socket is closed.</li>
 *   <li>Epoch fencing: a challenge below the highest epoch that ever reached ready is refused (needs_repair
 *       stale_epoch); a request whose epoch is not the session's is refused and NEVER run.</li>
 *   <li>Exactly-once: requests are run strictly in seq order (seq == ack+1); a duplicate delivery is only acked; a
 *       request id seen before is answered from the cache (replayed=true) and never run again. Responses and events
 *       are sequenced and kept in an outbox until the hub acks them, and resent after resume.</li>
 *   <li>Heartbeat every 15 s while charging or with the screen on, 60 s otherwise, with the beacon merged in.
 *       No frame from the hub for 2 x period + 5 s drops the socket. Redial with jittered backoff capped at 30 s.</li>
 * </ul>
 * Everything runs on the {@link Clock}'s single thread; public methods only post onto it, so they are thread-safe.
 */
public final class LinkClient {
    public static final long HEARTBEAT_ACTIVE_MS = 15000;
    public static final long HEARTBEAT_IDLE_MS = 60000;
    public static final long LIVENESS_SLACK_MS = 5000;
    public static final long HANDSHAKE_TIMEOUT_MS = 15000;
    public static final int CACHE_LIMIT = 4096;
    public static final int OUTBOX_LIMIT = 4096;
    public static final int PENDING_EVENTS_LIMIT = 32;
    public static final int CLOSE_REPAIR = 4001, CLOSE_PROTOCOL = 4002, CLOSE_LIVENESS = 4003, CLOSE_OUTBOX = 4004;

    public enum Phase { IDLE, DIALING, AWAIT_CHALLENGE, AWAIT_READY, READY }

    private static final class Cached {
        final String rpcText;
        final Object responseRpc;
        Cached(String rpcText, Object responseRpc) { this.rpcText = rpcText; this.responseRpc = responseRpc; }
    }

    private final Clock clock;
    private final Transport transport;
    private final Identity identity;
    private final TrustStore trust;
    private final LinkSession session;
    private final LinkEnv env;
    private final SecureRandom secure;
    private final Random jitter;
    private final String helperVersion;
    private final int sdk;
    private final String instance;

    // connection
    private String url;
    private boolean stopped = true;
    private Transport.Socket sock;
    private int gen;
    private Phase phase = Phase.IDLE;
    private String phoneNonce;
    private long pendingEpoch;
    private String myKeyId;
    private int attempt;
    private long lastRedialDelayMs = -1;
    private long lastInboundMs;
    private long lastHeartbeatMs;
    private Clock.Task dialTask, heartbeatTask, livenessTask;

    // stream (per instance and epoch)
    private long epoch;
    private long rxAcked;
    private long txSeq;
    private final TreeMap<Long, String> outbox = new TreeMap<Long, String>();
    private final LinkedHashMap<String, Cached> cache = new LinkedHashMap<String, Cached>();
    private final List<Map<String, Object>> pendingEvents = new ArrayList<Map<String, Object>>();

    // counters (status and tests)
    private long executed, replayedCount, refusedStale, heartbeatsSent, resentCount;

    public LinkClient(Clock clock, Transport transport, Identity identity, TrustStore trust, LinkSession session, LinkEnv env,
                      SecureRandom secure, Random jitter, String helperVersion, int sdk) {
        this.clock = clock;
        this.transport = transport;
        this.identity = identity;
        this.trust = trust;
        this.session = session;
        this.env = env;
        this.secure = secure;
        this.jitter = jitter;
        this.helperVersion = helperVersion;
        this.sdk = sdk;
        byte[] b = new byte[16];
        secure.nextBytes(b);
        StringBuilder s = new StringBuilder();
        for (byte x : b) s.append(Character.forDigit((x >> 4) & 0xf, 16)).append(Character.forDigit(x & 0xf, 16));
        this.instance = s.toString();
    }

    // ------------------------------------------------------------------ public, thread-safe

    /**
     * Start (or retarget) the link. The host is checked here, synchronously: anything outside 100.64.0.0/10 (or a
     * *.ts.net name, whose resolved addresses are filtered at dial time) is refused and nothing is dialled.
     */
    public void start(final String host, final int port) throws TailnetGuard.Refused {
        TailnetGuard.checkHost(host);
        final String u;
        try {
            u = HubSpec.parse(host + ":" + port, null).url(); // the one place a hub URL is built (ws://host:port/helper)
        } catch (HubSpec.Invalid e) {
            throw new TailnetGuard.Refused(e.getMessage());
        }
        post(new Runnable() { public void run() {
            boolean retarget = url != null && !url.equals(u);
            url = u;
            stopped = false;
            if (retarget) drop(1000, "hub address changed");
            else if (sock == null && dialTask == null) dial();
        } });
    }

    public void stop() {
        post(new Runnable() { public void run() {
            stopped = true;
            cancel(dialTask);
            dialTask = null;
            if (sock != null) {
                Transport.Socket s = sock;
                disconnected();
                s.close(1000, "stopped");
            }
        } });
    }

    /** The default network came back: forget the backoff and dial now if we are down. */
    public void onNetworkAvailable() {
        post(new Runnable() { public void run() {
            attempt = 0;
            if (!stopped && sock == null) {
                cancel(dialTask);
                dialTask = null;
                dial();
            }
        } });
    }

    /** Charging or screen state changed: the heartbeat period may switch between 15 s and 60 s. */
    public void onPowerChanged() {
        post(new Runnable() { public void run() {
            if (phase != Phase.READY) return;
            long due = lastHeartbeatMs + periodMs();
            scheduleHeartbeat(Math.max(0, due - clock.nowMs()));
            armLiveness();
        } });
    }

    /** A sequenced phone event (untrusted_bssid). Held while not ready, sent after the next ready. */
    public void sendEvent(final Map<String, Object> frame) {
        post(new Runnable() { public void run() {
            if (phase == Phase.READY) {
                sendSequenced(frame);
            } else {
                pendingEvents.add(frame);
                while (pendingEvents.size() > PENDING_EVENTS_LIMIT) pendingEvents.remove(0);
            }
        } });
    }

    // Read-only views, for status, the notification and tests. Call on the clock thread for an exact value.
    public Phase phase() { return phase; }
    public long epoch() { return epoch; }
    public long rxAcked() { return rxAcked; }
    public long txSeq() { return txSeq; }
    public int outboxSize() { return outbox.size(); }
    public int cacheSize() { return cache.size(); }
    public long executed() { return executed; }
    public long replayed() { return replayedCount; }
    public long refusedStale() { return refusedStale; }
    public long heartbeatsSent() { return heartbeatsSent; }
    /** Outbox frames sent again after a resume (responses or events the hub had not received). */
    public long resent() { return resentCount; }
    public long lastRedialDelayMs() { return lastRedialDelayMs; }
    public String instance() { return instance; }
    public boolean connected() { return sock != null && phase != Phase.DIALING; }

    /** Current heartbeat period: 15 s while charging or the screen is on, else 60 s. */
    public long periodMs() { return (env.charging() || env.screenOn()) ? HEARTBEAT_ACTIVE_MS : HEARTBEAT_IDLE_MS; }

    // ------------------------------------------------------------------ connection

    private void post(Runnable r) { clock.schedule(0, r); }

    private static void cancel(Clock.Task t) { if (t != null) t.cancel(); }

    private void dial() {
        dialTask = null;
        if (stopped || sock != null || url == null) return;
        final int g = ++gen;
        phase = Phase.DIALING;
        sock = transport.dial(url, new Transport.Listener() {
            public void onOpen(final Transport.Socket s) { post(new Runnable() { public void run() { opened(g, s); } }); }
            public void onText(final Transport.Socket s, final String text) { post(new Runnable() { public void run() { if (g == gen) receive(text); } }); }
            public void onClosed(final Transport.Socket s) { post(new Runnable() { public void run() { if (g == gen && sock != null) { disconnected(); redial(); } } }); }
        });
        armLiveness();
    }

    private void opened(int g, Transport.Socket s) {
        if (g != gen || sock == null) {
            s.close(1000, "superseded");
            return;
        }
        sock = s;
        phase = Phase.AWAIT_CHALLENGE;
        pendingEpoch = 0;
        lastInboundMs = clock.nowMs();
        session.onDisconnect(); // every connection starts unauthenticated; needs_repair stays sticky
        env.onStateChanged();
        byte[] spki;
        try {
            spki = identity.publicKeySpki();
        } catch (GeneralSecurityException | RuntimeException e) {
            repair(LinkProto.R_KEY_UNAVAILABLE);
            return;
        }
        myKeyId = LinkCrypto.keyId(spki);
        phoneNonce = LinkCrypto.newNonce(secure);
        Map<String, Object> h = LinkProto.frame(LinkProto.HELLO);
        h.put("key_id", myKeyId);
        h.put("alg", Identity.ALG);
        h.put("phone_nonce", phoneNonce);
        h.put("instance", instance);
        h.put("helper_version", helperVersion);
        h.put("sdk", Long.valueOf(sdk));
        boolean repairState = session.state() == LinkSession.State.NEEDS_REPAIR;
        h.put("state", repairState ? "needs_repair" : "pending_auth");
        String f = session.failure();
        if (repairState && f != null && (LinkProto.PHONE_FIX.containsKey(f) || LinkProto.HUB_FIX.containsKey(f))) h.put("repair_reason", f);
        h.put("epoch_seen", Long.valueOf(trust.maxEpoch()));
        send(h);
        armLiveness();
    }

    /** Socket gone (or being closed by us): forget it, stop timers, session back to pending (needs_repair sticky). */
    private void disconnected() {
        sock = null;
        gen++;
        phase = Phase.IDLE;
        cancel(heartbeatTask);
        heartbeatTask = null;
        cancel(livenessTask);
        livenessTask = null;
        session.onDisconnect();
        env.onStateChanged();
    }

    private void redial() {
        if (stopped || dialTask != null) return;
        long d = Backoff.delayMs(attempt++, jitter);
        lastRedialDelayMs = d;
        dialTask = clock.schedule(d, new Runnable() { public void run() { dial(); } });
    }

    private void drop(int code, String reason) {
        Transport.Socket s = sock;
        if (s == null) {
            if (!stopped && dialTask == null) redial();
            return;
        }
        disconnected();
        s.close(code, reason);
        redial();
    }

    private void repair(String reason) {
        session.linkNeedsRepair(reason, LinkProto.PHONE_FIX.get(reason));
        send(LinkProto.needsRepair(reason, Math.max(pendingEpoch, 0)));
        drop(CLOSE_REPAIR, "needs_repair");
    }

    private void send(Map<String, Object> frame) {
        Transport.Socket s = sock;
        if (s != null) s.send(LinkProto.write(frame));
    }

    // ------------------------------------------------------------------ receive

    private void receive(String text) {
        lastInboundMs = clock.nowMs();
        Map<String, Object> m;
        try {
            m = LinkProto.parseFromHub(text);
        } catch (LinkProto.BadFrame e) {
            if (LinkProto.CHALLENGE.equals(e.type) && phase == Phase.AWAIT_CHALLENGE) {
                repair(LinkProto.R_BAD_CHALLENGE);
            } else {
                send(LinkProto.refused(epoch, LinkProto.X_BAD_REQUEST, e.id, null, null));
            }
            return;
        }
        String t = (String) m.get("t");
        if (t.equals(LinkProto.CHALLENGE)) onChallenge(m);
        else if (t.equals(LinkProto.READY)) onReady(m);
        else if (t.equals(LinkProto.NEEDS_REPAIR)) onHubRepair(m);
        else if (t.equals(LinkProto.REQUEST)) onRequest(m);
        else if (t.equals(LinkProto.SEGMENT)) onRequest(LinkProto.segmentAsRequest(m)); // runner.segment (2.14)
        else if (t.equals(LinkProto.ACK)) onAck(m);
        else if (t.equals(LinkProto.RESUME)) onResume(m);
    }

    private void onChallenge(Map<String, Object> m) {
        if (phase != Phase.AWAIT_CHALLENGE) {
            drop(CLOSE_PROTOCOL, "unexpected challenge");
            return;
        }
        long e = (Long) m.get("epoch");
        pendingEpoch = e;
        String nonce = (String) m.get("nonce");
        String serial = (String) m.get("serial");
        byte[] hubKey = trust.hubKeySpki();
        String mySerial = trust.serial();
        if (hubKey == null || mySerial == null) {
            repair(LinkProto.R_HUB_KEY_MISSING);
            return;
        }
        if (!phoneNonce.equals(m.get("phone_nonce"))) {
            repair(LinkProto.R_BAD_CHALLENGE); // not an answer to this connection's hello: a replayed challenge
            return;
        }
        if (!LinkCrypto.keyId(hubKey).equals(m.get("hub_key_id"))
                || !LinkCrypto.verify(hubKey, LinkCrypto.hubChallengeMessage(nonce, phoneNonce, e, serial, myKeyId),
                        LinkCrypto.unb64((String) m.get("hub_sig")))) {
            repair(LinkProto.R_BAD_HUB_SIGNATURE);
            return;
        }
        if (!mySerial.equals(serial)) {
            repair(LinkProto.R_SERIAL_MISMATCH);
            return;
        }
        if (e < trust.maxEpoch()) {
            repair(LinkProto.R_STALE_EPOCH);
            return;
        }
        byte[] sig;
        try {
            sig = identity.sign(LinkCrypto.phoneResponseMessage(nonce, phoneNonce, e, mySerial, LinkCrypto.keyId(hubKey)));
        } catch (GeneralSecurityException | RuntimeException ex) {
            repair(LinkProto.R_KEY_UNAVAILABLE);
            return;
        }
        Map<String, Object> r = LinkProto.frame(LinkProto.CHALLENGE_RESPONSE);
        r.put("epoch", Long.valueOf(e));
        r.put("nonce", nonce);
        r.put("key_id", myKeyId);
        r.put("signature", LinkCrypto.b64(sig));
        send(r);
        phase = Phase.AWAIT_READY;
    }

    private void onReady(Map<String, Object> m) {
        long e = (Long) m.get("epoch");
        if (phase != Phase.AWAIT_READY || e != pendingEpoch || !myKeyId.equals(m.get("key_id"))) {
            drop(CLOSE_PROTOCOL, "unexpected ready");
            return;
        }
        if (e != epoch) { // a new stream: new epoch (or the first ready of this process)
            epoch = e;
            rxAcked = 0;
            txSeq = 0;
            outbox.clear();
        }
        if (e > trust.maxEpoch()) trust.setMaxEpoch(e);
        session.linkReady();
        attempt = 0;
        phase = Phase.READY;
        Map<String, Object> r = LinkProto.frame(LinkProto.RESUME);
        r.put("epoch", Long.valueOf(epoch));
        r.put("last_acked", Long.valueOf(rxAcked));
        send(r);
        List<Map<String, Object>> evs = new ArrayList<Map<String, Object>>(pendingEvents);
        pendingEvents.clear();
        for (Map<String, Object> ev : evs) sendSequenced(ev);
        scheduleHeartbeat(0);
        armLiveness();
        env.onStateChanged();
    }

    private void onHubRepair(Map<String, Object> m) {
        String reason = (String) m.get("reason");
        session.linkNeedsRepair(reason, LinkProto.HUB_FIX.get(reason));
        drop(CLOSE_REPAIR, "hub says needs_repair");
    }

    @SuppressWarnings("unchecked")
    private void onRequest(Map<String, Object> m) {
        String id = (String) m.get("id");
        long reqSeq = (Long) m.get("seq");
        long reqEpoch = (Long) m.get("epoch");
        if (phase != Phase.READY) {
            send(LinkProto.refused(0, LinkProto.X_NOT_READY, id, reqSeq, reqEpoch));
            return;
        }
        if (reqEpoch != epoch) { // epoch fencing: never run, never advances the stream
            if (reqEpoch < epoch) refusedStale++;
            send(LinkProto.refused(epoch, reqEpoch < epoch ? LinkProto.X_STALE_EPOCH : LinkProto.X_EPOCH_AHEAD, id, reqSeq, reqEpoch));
            return;
        }
        if (reqSeq <= rxAcked) { // a second delivery of something already run: its response is in the outbox or acked
            sendAck();
            return;
        }
        if (reqSeq != rxAcked + 1) {
            send(LinkProto.refused(epoch, LinkProto.X_OUT_OF_ORDER, id, reqSeq, reqEpoch));
            Map<String, Object> r = LinkProto.frame(LinkProto.RESUME);
            r.put("epoch", Long.valueOf(epoch));
            r.put("last_acked", Long.valueOf(rxAcked));
            send(r);
            return;
        }
        rxAcked = reqSeq;
        Map<String, Object> rpc = (Map<String, Object>) m.get("rpc");
        String rpcText = Json.write(rpc);
        Cached c = cache.get(id);
        if (c != null) {
            if (c.rpcText.equals(rpcText)) {
                replayedCount++;
                respond(id, c.responseRpc, true);
            } else {
                respond(id, Frames.error(id, ErrorCodes.INVALID_REQUEST, "id already used by a different request", null), false);
            }
            return;
        }
        Object out;
        try {
            executed++;
            out = Json.parse(new String(env.execute(rpcText.getBytes(StandardCharsets.UTF_8)), StandardCharsets.UTF_8));
        } catch (Json.ParseException | RuntimeException ex) {
            out = Frames.error(id, ErrorCodes.INTERNAL_ERROR, "internal error", null);
        }
        if (cacheable(out)) {
            cache.put(id, new Cached(rpcText, out));
            if (cache.size() > CACHE_LIMIT) {
                Iterator<String> it = cache.keySet().iterator();
                it.next();
                it.remove();
            }
        }
        respond(id, out, false);
    }

    /**
     * Gate refusals guarantee nothing ran, so they are not cached: the same id may run once the gate opens. A
     * TRANSIENT answer (busy, not ready, runner timeout) is not cached either, so it cannot pin the id.
     */
    @SuppressWarnings("unchecked")
    private static boolean cacheable(Object rpcResponse) {
        if (!(rpcResponse instanceof Map)) return false;
        Object err = ((Map<String, Object>) rpcResponse).get("error");
        if (!(err instanceof Map)) return true;
        Object code = ((Map<String, Object>) err).get("code");
        long c = code instanceof Long ? (Long) code : 0;
        return c != ErrorCodes.NEEDS_REPAIR && c != ErrorCodes.AUTH_REQUIRED && c != ErrorCodes.NOTIFICATION_HIDDEN
                && c != ErrorCodes.TRANSIENT;
    }

    private void respond(String id, Object rpc, boolean replayed) {
        Map<String, Object> r = LinkProto.frame(LinkProto.RESPONSE);
        r.put("id", id);
        r.put("replayed", Boolean.valueOf(replayed));
        r.put("rpc", rpc);
        sendSequenced(r);
    }

    /** Stamp epoch, seq and ack, keep in the outbox until acked, send. Field order follows link.json. */
    private void sendSequenced(Map<String, Object> body) {
        Map<String, Object> f = new LinkedHashMap<String, Object>();
        f.put("v", Long.valueOf(LinkProto.VERSION));
        f.put("t", body.get("t"));
        f.put("epoch", Long.valueOf(epoch));
        f.put("seq", Long.valueOf(++txSeq));
        f.put("ack", Long.valueOf(rxAcked));
        for (Map.Entry<String, Object> e : body.entrySet()) if (!f.containsKey(e.getKey())) f.put(e.getKey(), e.getValue());
        String text = LinkProto.write(f);
        outbox.put(Long.valueOf(txSeq), text);
        Transport.Socket s = sock;
        if (s != null) s.send(text);
        if (outbox.size() > OUTBOX_LIMIT) drop(CLOSE_OUTBOX, "hub is not acking"); // kept; resent after resume
    }

    private void sendAck() {
        Map<String, Object> a = LinkProto.frame(LinkProto.ACK);
        a.put("epoch", Long.valueOf(epoch));
        a.put("ack", Long.valueOf(rxAcked));
        send(a);
    }

    private void onAck(Map<String, Object> m) {
        if ((Long) m.get("epoch") != epoch) return;
        trim((Long) m.get("ack"));
    }

    private void onResume(Map<String, Object> m) {
        if (phase != Phase.READY || (Long) m.get("epoch") != epoch) return;
        long last = (Long) m.get("last_acked");
        if (last > txSeq) txSeq = last; // the hub knows more than we do: never reuse a seq it has seen
        trim(last);
        Transport.Socket s = sock;
        if (s == null) return;
        for (String text : outbox.values()) {
            s.send(text);
            resentCount++;
        }
    }

    private void trim(long acked) {
        while (!outbox.isEmpty() && outbox.firstKey() <= acked) outbox.pollFirstEntry();
    }

    // ------------------------------------------------------------------ timers

    private void scheduleHeartbeat(long delay) {
        cancel(heartbeatTask);
        heartbeatTask = clock.schedule(delay, new Runnable() { public void run() { heartbeat(); } });
    }

    private void heartbeat() {
        heartbeatTask = null;
        if (phase != Phase.READY) return;
        long period = periodMs();
        Map<String, Object> h = LinkProto.frame(LinkProto.HEARTBEAT);
        h.put("epoch", Long.valueOf(epoch));
        h.put("ts_ms", Long.valueOf(clock.wallMs()));
        h.put("period_s", Long.valueOf(period / 1000));
        h.put("ack", Long.valueOf(rxAcked));
        h.put("tx_seq", Long.valueOf(txSeq));
        h.put("charging", Boolean.valueOf(env.charging()));
        h.put("screen_on", Boolean.valueOf(env.screenOn()));
        Map<String, Object> b = env.beacon();
        h.put("battery_pct", b.get("battery_pct"));
        h.put("adb_port", b.get("adb_port"));
        h.put("adb_port_source", b.containsKey("adb_port_source") ? b.get("adb_port_source") : "none");
        h.put("wifi_adb", b.get("wifi_adb"));
        h.put("a11y_enabled", b.get("a11y_enabled"));
        h.put("keeper", b.containsKey("keeper") ? b.get("keeper") : "absent");
        h.put("bssid_trusted", b.get("bssid_trusted"));
        send(h);
        heartbeatsSent++;
        lastHeartbeatMs = clock.nowMs();
        scheduleHeartbeat(period);
    }

    private long livenessTimeoutMs() {
        return phase == Phase.READY ? 2 * periodMs() + LIVENESS_SLACK_MS : HANDSHAKE_TIMEOUT_MS;
    }

    /** One pending check at a time; it re-arms itself at the current deadline instead of on every frame. */
    private void armLiveness() {
        cancel(livenessTask);
        final int g = gen;
        long since = phase == Phase.DIALING ? 0 : clock.nowMs() - lastInboundMs;
        livenessTask = clock.schedule(Math.max(0, livenessTimeoutMs() - since), new Runnable() { public void run() {
            livenessTask = null;
            if (g != gen || sock == null) return;
            if (phase == Phase.DIALING || clock.nowMs() - lastInboundMs >= livenessTimeoutMs()) drop(CLOSE_LIVENESS, "hub silent");
            else armLiveness();
        } });
    }
}
