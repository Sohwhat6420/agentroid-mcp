package dev.agentroid.helper.link.net;

/**
 * The dial-out seam. OkHttpTransport is the real one (an OkHttp WebSocket CLIENT; nothing listens); the tests use an
 * in-process fake hub. Callbacks may arrive on any thread; LinkClient moves them onto its Clock thread.
 */
public interface Transport {
    interface Socket {
        /** Queue a text frame; false when the socket is already closed. */
        boolean send(String text);
        void close(int code, String reason);
    }

    interface Listener {
        void onOpen(Socket s);
        void onText(Socket s, String text);
        /** Closed or failed; called at most once per socket. */
        void onClosed(Socket s);
    }

    /** Start dialling url. Must not block; the outcome arrives on the listener. */
    Socket dial(String url, Listener l);
}
