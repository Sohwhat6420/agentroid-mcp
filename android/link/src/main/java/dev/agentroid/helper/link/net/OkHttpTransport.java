package dev.agentroid.helper.link.net;

import java.net.InetAddress;
import java.net.UnknownHostException;
import java.util.List;
import java.util.concurrent.TimeUnit;

import okhttp3.Dns;
import okhttp3.OkHttpClient;
import okhttp3.Request;
import okhttp3.Response;
import okhttp3.WebSocket;
import okhttp3.WebSocketListener;
import okio.ByteString;

/**
 * The real transport: an OkHttp WebSocket CLIENT. It only dials out; there is no server socket anywhere in the app.
 * The OkHttpClient's Dns is {@link TailnetDns}, so even a *.ts.net name can only ever connect to 100.64.0.0/10, and a
 * literal outside that range was already refused by LinkClient.start. No proxy (a proxy would dial something else).
 * OkHttp's own ping frames (every 15 s) catch a half-open TCP connection between heartbeats.
 */
public final class OkHttpTransport implements Transport {
    /** Resolves through the system resolver, then keeps only tailnet addresses (none left = refused). */
    public static final class TailnetDns implements Dns {
        private final Dns delegate;
        public TailnetDns(Dns delegate) { this.delegate = delegate; }
        @Override public List<InetAddress> lookup(String hostname) throws UnknownHostException {
            return TailnetGuard.onlyTailnet(hostname, delegate.lookup(hostname));
        }
    }

    private final OkHttpClient http;

    public OkHttpTransport() {
        this(new OkHttpClient.Builder()
                .dns(new TailnetDns(Dns.SYSTEM))
                .proxy(java.net.Proxy.NO_PROXY)
                .pingInterval(15, TimeUnit.SECONDS)
                .connectTimeout(5, TimeUnit.SECONDS)
                .readTimeout(0, TimeUnit.SECONDS)
                .retryOnConnectionFailure(false)
                .build());
    }

    OkHttpTransport(OkHttpClient http) { this.http = http; }

    public OkHttpClient client() { return http; }

    /** Built before dialling, so a callback that fires before newWebSocket returns still has a socket to name. */
    private static final class Sock implements Socket {
        volatile WebSocket ws;
        @Override public boolean send(String text) {
            WebSocket w = ws;
            return w != null && w.send(text);
        }
        @Override public void close(int code, String reason) {
            WebSocket w = ws;
            if (w != null && !w.close(code, reason)) w.cancel();
        }
    }

    @Override
    public Socket dial(String url, final Listener l) {
        final Sock s = new Sock();
        s.ws = http.newWebSocket(new Request.Builder().url(url).build(), new WebSocketListener() {
            private boolean closed;
            @Override public void onOpen(WebSocket w, Response r) { s.ws = w; l.onOpen(s); }
            @Override public void onMessage(WebSocket w, String text) { l.onText(s, text); }
            @Override public void onMessage(WebSocket w, ByteString bytes) { l.onText(s, bytes.utf8()); }
            @Override public void onClosing(WebSocket w, int code, String reason) { w.close(1000, null); }
            @Override public synchronized void onClosed(WebSocket w, int code, String reason) { once(); }
            @Override public synchronized void onFailure(WebSocket w, Throwable t, Response r) { once(); }
            private void once() {
                if (closed) return;
                closed = true;
                l.onClosed(s);
            }
        });
        return s;
    }
}
