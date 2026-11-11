package dev.agentroid.helper.ime;

import android.inputmethodservice.InputMethodService;
import android.os.Handler;
import android.os.Looper;
import android.view.inputmethod.InputConnection;

import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;

import dev.agentroid.helper.core.PortRegistry;
import dev.agentroid.helper.core.Ports;

/**
 * A keyboard with no keys: it shows nothing and only commits text that arrives over the link (type_text), so any
 * Unicode string, e.g. "café 東京", is typed exactly as sent. It has to be enabled in system settings and
 * be the current keyboard; while it runs it publishes its port to the PortRegistry, and type_text fails with
 * ime_not_active when it is not there. The committed text is never logged.
 */
public class HelperInputMethodService extends InputMethodService {
    private final Handler main = new Handler(Looper.getMainLooper());

    private final Ports.Ime port = new Ports.Ime() {
        @Override
        public void commitText(final String text) throws Ports.PortException {
            final CountDownLatch done = new CountDownLatch(1);
            final AtomicReference<String> failure = new AtomicReference<String>();
            main.post(new Runnable() {
                @Override public void run() {
                    try {
                        InputConnection ic = getCurrentInputConnection();
                        if (ic == null) failure.set("ime_no_field");
                        else if (!ic.commitText(text, 1)) failure.set("ime_commit_refused");
                    } catch (RuntimeException e) {
                        failure.set("ime_commit_failed");
                    } finally {
                        done.countDown();
                    }
                }
            });
            try {
                if (!done.await(5, TimeUnit.SECONDS)) throw new Ports.PortException("ime_timeout", "the keyboard did not answer in time");
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
                throw new Ports.PortException("interrupted", "interrupted while typing");
            }
            String f = failure.get();
            if (f != null) {
                throw new Ports.PortException(f, "ime_no_field".equals(f)
                        ? "no text field is focused, so there is nowhere to type"
                        : "the text field did not accept the text");
            }
        }
    };

    @Override
    public void onCreate() {
        super.onCreate();
        PortRegistry.shared().setIme(port);
    }

    @Override
    public void onDestroy() {
        PortRegistry.shared().clearIme(port);
        super.onDestroy();
    }

    /** No on-screen keyboard: the service only relays committed text. */
    @Override
    public boolean onEvaluateInputViewShown() { return false; }
}
