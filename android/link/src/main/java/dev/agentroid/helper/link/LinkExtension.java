package dev.agentroid.helper.link;

import android.content.Context;

import java.util.Map;

import dev.agentroid.helper.core.Dispatcher;

/**
 * A module that rides on the link without :link depending on it. :keeper is in the `full` flavour only, so LinkService
 * finds it by class NAME (the same pattern AndroidEnv uses for the IME): {@link #KEEPER_CLASS}, if present.
 */
public interface LinkExtension {
    String KEEPER_CLASS = "dev.agentroid.helper.keeper.KeeperExtension";

    interface Host {
        /** Send a sequenced event (link.json untrusted_bssid); held until the link is ready. */
        void event(Map<String, Object> body);

        /** The serial the hub paired this phone as, or null. */
        String serial();
    }

    /** Called once from LinkService.onCreate, on the main thread. May register Dispatcher methods. */
    void attach(Context c, Dispatcher d, Host host);

    /** Beacon fields merged into the heartbeat. Called on the link thread; must be quick and not throw. */
    Map<String, Object> beacon();

    void detach();
}
