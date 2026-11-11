package dev.agentroid.helper.link.net;

import java.security.SecureRandom;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.TreeMap;

import dev.agentroid.helper.core.Json;

/**
 * In-process hub AND network for the link tests. It is a minimal but complete hub side of protocol v1 (the reference
 * the hub side can read next to link.json): challenge signed with the hub key, phone-signature check, ready, resume,
 * sequenced requests with an outbox until the phone's ack, response dedupe by seq, acks, heartbeat acks.
 *
 * The network delivers each frame after latencyMs on the shared FakeClock. drop() loses everything in flight in
 * both directions, like a Wi-Fi toggle, and the phone then sees the close.
 */
final class FakeHub implements Transport {
    final FakeClock clock;
    final SoftwareIdentity hubKey;
    final SecureRandom rnd = new SecureRandom(new byte[] {1});
    long latencyMs = 1;
    long epoch;
    String serial;
    byte[] phoneKey;            // the phone key the hub paired (read over adb in real life)
    SoftwareIdentity signAs;    // null: sign challenges with hubKey; else with this (wrong) key
    boolean refuseConnections;
    boolean silent;             // stop answering heartbeats (liveness test)
    boolean revoked;            // answer the next challenge_response with needs_repair key_revoked
    int window = 32;
    int ackEvery = 8;
    String forcePhoneNonce;     // put this phone_nonce in the next challenge (replayed-challenge test)
    Runnable afterReceive;      // test hook, run after every frame the hub handles

    // network
    final class Conn {
        final Listener phone;
        boolean open = true;
        final Socket phoneSide;
        String phoneNonce;
        boolean ready;
        Conn(Listener phone) {
            this.phone = phone;
            this.phoneSide = new Socket() {
                public boolean send(final String text) {
                    if (!open) return false;
                    sent.add(text);
                    clock.schedule(latencyMs, new Runnable() { public void run() { if (open) receive(Conn.this, text); } });
                    return true;
                }
                public void close(int code, String reason) {
                    if (!open) return;
                    // a graceful close: frames already queued still arrive, then the socket is gone
                    clock.schedule(latencyMs, new Runnable() { public void run() { Conn.this.close(); } });
                }
            };
        }
        void toPhone(final String text) {
            if (!open) return;
            hubSent.add(text);
            clock.schedule(latencyMs, new Runnable() { public void run() { if (open) phone.onText(phoneSide, text); } });
        }
        void close() {
            if (!open) return;
            open = false;
            if (current == this) current = null;
            clock.schedule(latencyMs, new Runnable() { public void run() { phone.onClosed(phoneSide); } });
        }
    }

    Conn current;
    final List<Long> dialTimes = new ArrayList<Long>();
    final List<String> sent = new ArrayList<String>();      // every frame the phone sent
    final List<String> hubSent = new ArrayList<String>();   // every frame the hub sent
    int drops;
    boolean recordFrames = true;

    // hub-side stream (per phone instance and epoch)
    String instance;
    long txSeq;                                             // hub -> phone
    long rxAcked;                                           // phone -> hub, in order
    final TreeMap<Long, String> outbox = new TreeMap<Long, String>();
    final List<String[]> queue = new ArrayList<String[]>(); // {id, method, paramsJson} not yet sent
    int queueHead;

    // what the hub application saw
    final Map<String, Object> responses = new LinkedHashMap<String, Object>();
    final Map<String, Integer> responsesPerId = new HashMap<String, Integer>();
    final List<Map<String, Object>> events = new ArrayList<Map<String, Object>>();
    final Map<String, Integer> refusals = new HashMap<String, Integer>();
    final List<Map<String, Object>> repairs = new ArrayList<Map<String, Object>>();
    final List<Map<String, Object>> heartbeats = new ArrayList<Map<String, Object>>();
    final List<Long> heartbeatTimes = new ArrayList<Long>();
    int wireDuplicates, appDuplicates, replayedSeen, readies, resentRequests;

    FakeHub(FakeClock clock, long hubSeed, long epoch, String serial) {
        this.clock = clock;
        this.hubKey = new SoftwareIdentity(hubSeed);
        this.epoch = epoch;
        this.serial = serial;
    }

    byte[] hubSpki() { return hubKey.publicKeySpki(); }

    // ------------------------------------------------------------------ Transport

    @Override
    public Socket dial(String url, Listener l) {
        dialTimes.add(Long.valueOf(clock.nowMs()));
        final Conn c = new Conn(l);
        if (refuseConnections) {
            c.open = false;
            clock.schedule(latencyMs, new Runnable() { public void run() { c.phone.onClosed(c.phoneSide); } });
            return c.phoneSide;
        }
        if (current != null) current.close();
        current = c;
        clock.schedule(latencyMs, new Runnable() { public void run() { if (c.open) c.phone.onOpen(c.phoneSide); } });
        return c.phoneSide;
    }

    /** Lose the connection and everything in flight. */
    void drop() {
        if (current == null) return;
        drops++;
        current.close();
    }

    // ------------------------------------------------------------------ hub logic

    void enqueue(String id, String method, String paramsJson) { queue.add(new String[] {id, method, paramsJson}); }

    int pendingToSend() { return queue.size() - queueHead; }

    void receive(Conn c, String text) {
        handle(c, text);
        if (afterReceive != null) afterReceive.run();
    }

    @SuppressWarnings("unchecked")
    private void handle(Conn c, String text) {
        Map<String, Object> m;
        try {
            m = (Map<String, Object>) Json.parse(text);
        } catch (Json.ParseException e) {
            throw new AssertionError("phone sent bad JSON: " + text);
        }
        String t = (String) m.get("t");
        if (t.equals("hello")) {
            c.phoneNonce = (String) m.get("phone_nonce");
            String inst = (String) m.get("instance");
            if (!inst.equals(instance)) { // a new phone process: new stream
                instance = inst;
                txSeq = 0;
                rxAcked = 0;
                outbox.clear();
            }
            String nonce = LinkCrypto.newNonce(rnd);
            String keyId = (String) m.get("key_id");
            SoftwareIdentity signer = signAs != null ? signAs : hubKey;
            Map<String, Object> ch = LinkProto.frame("challenge");
            ch.put("epoch", Long.valueOf(epoch));
            ch.put("nonce", nonce);
            String echo = forcePhoneNonce != null ? forcePhoneNonce : c.phoneNonce;
            ch.put("phone_nonce", echo);
            ch.put("serial", serial);
            ch.put("hub_key_id", LinkCrypto.keyId(hubKey.publicKeySpki()));
            try {
                ch.put("hub_sig", LinkCrypto.b64(signer.sign(LinkCrypto.hubChallengeMessage(nonce, echo, epoch, serial, keyId))));
            } catch (Exception e) {
                throw new AssertionError(e);
            }
            c.toPhone(Json.write(ch));
        } else if (t.equals("challenge_response")) {
            boolean good = phoneKey != null
                    && LinkCrypto.keyId(phoneKey).equals(m.get("key_id"))
                    && LinkCrypto.verify(phoneKey, LinkCrypto.phoneResponseMessage((String) m.get("nonce"), c.phoneNonce,
                            (Long) m.get("epoch"), serial, LinkCrypto.keyId(hubKey.publicKeySpki())), LinkCrypto.unb64((String) m.get("signature")));
            if (!good || revoked) {
                String reason = revoked ? "key_revoked" : (phoneKey == null ? "unknown_device_key" : "bad_phone_signature");
                Map<String, Object> r = LinkProto.frame("needs_repair");
                r.put("from", "hub");
                r.put("reason", reason);
                r.put("fix", LinkProto.HUB_FIX.get(reason));
                r.put("epoch", Long.valueOf(epoch));
                c.toPhone(Json.write(r));
                return;
            }
            Map<String, Object> r = LinkProto.frame("ready");
            r.put("epoch", Long.valueOf(epoch));
            r.put("key_id", m.get("key_id"));
            c.toPhone(Json.write(r));
            c.ready = true;
            readies++;
            Map<String, Object> rs = LinkProto.frame("resume");
            rs.put("epoch", Long.valueOf(epoch));
            rs.put("last_acked", Long.valueOf(rxAcked));
            c.toPhone(Json.write(rs));
        } else if (t.equals("resume")) {
            trim((Long) m.get("last_acked"));
            for (String f : outbox.values()) { // resend what the phone has not got, in order, same seq
                c.toPhone(f);
                resentRequests++;
            }
            pump(c);
        } else if (t.equals("ack")) {
            trim((Long) m.get("ack"));
            pump(c);
        } else if (t.equals("response") || t.equals("untrusted_bssid")) {
            long seq = (Long) m.get("seq");
            trim((Long) m.get("ack"));
            if (seq <= rxAcked) {
                wireDuplicates++;
            } else if (seq == rxAcked + 1) {
                rxAcked = seq;
                if (t.equals("untrusted_bssid")) {
                    events.add(m);
                } else {
                    String id = (String) m.get("id");
                    if (Boolean.TRUE.equals(m.get("replayed"))) replayedSeen++;
                    Integer n = responsesPerId.get(id);
                    responsesPerId.put(id, n == null ? 1 : n + 1);
                    if (responses.containsKey(id)) appDuplicates++;
                    else responses.put(id, m.get("rpc"));
                }
                if (rxAcked % ackEvery == 0) ack(c);
            } else {
                Map<String, Object> rs = LinkProto.frame("resume"); // a gap: ask for a resend
                rs.put("epoch", Long.valueOf(epoch));
                rs.put("last_acked", Long.valueOf(rxAcked));
                c.toPhone(Json.write(rs));
            }
            pump(c);
        } else if (t.equals("heartbeat")) {
            heartbeats.add(m);
            heartbeatTimes.add(Long.valueOf(clock.nowMs()));
            if (!silent) ack(c);
        } else if (t.equals("refused")) {
            String reason = (String) m.get("reason");
            Integer n = refusals.get(reason);
            refusals.put(reason, n == null ? 1 : n + 1);
        } else if (t.equals("needs_repair")) {
            repairs.add(m);
        }
    }

    private void ack(Conn c) {
        Map<String, Object> a = LinkProto.frame("ack");
        a.put("epoch", Long.valueOf(epoch));
        a.put("ack", Long.valueOf(rxAcked));
        c.toPhone(Json.write(a));
    }

    private void trim(long acked) {
        while (!outbox.isEmpty() && outbox.firstKey() <= acked) outbox.pollFirstEntry();
    }

    /** Send queued requests while fewer than window are unacked. */
    void pump(Conn c) {
        if (c == null || !c.ready || !c.open) return;
        while (outbox.size() < window && queueHead < queue.size()) {
            String[] q = queue.get(queueHead++);
            String f = requestFrame(epoch, ++txSeq, q[0], q[1], q[2]);
            outbox.put(Long.valueOf(txSeq), f);
            c.toPhone(f);
        }
    }

    static String requestFrame(long epoch, long seq, String id, String method, String paramsJson) {
        return "{\"v\":1,\"t\":\"request\",\"epoch\":" + epoch + ",\"seq\":" + seq + ",\"id\":\"" + id + "\",\"rpc\":{\"jsonrpc\":\"2.0\",\"id\":\""
                + id + "\",\"method\":\"" + method + "\"" + (paramsJson == null ? "" : ",\"params\":" + paramsJson) + "}}";
    }

    /** Send a raw frame on the current connection (fencing and replay tests). */
    void raw(String frame) {
        if (current == null) throw new AssertionError("no connection");
        current.toPhone(frame);
    }

    void pumpNow() { pump(current); }

    boolean ready() { return current != null && current.ready; }

    @SuppressWarnings("unchecked")
    List<Map<String, Object>> phoneFrames(String type) {
        List<Map<String, Object>> out = new ArrayList<Map<String, Object>>();
        for (String s : sent) {
            try {
                Map<String, Object> m = (Map<String, Object>) Json.parse(s);
                if (type == null || type.equals(m.get("t"))) out.add(m);
            } catch (Json.ParseException e) {
                throw new AssertionError(e);
            }
        }
        return out;
    }
}
