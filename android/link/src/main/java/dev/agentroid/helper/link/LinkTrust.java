package dev.agentroid.helper.link;

import android.content.Context;

import dev.agentroid.helper.link.net.TrustStore;

/**
 * The one public door to the paired hub's trust: the runner's SegmentVerifier reads the pinned
 * hub key, the serial and the epoch from the same HubStore the link pairs into, so a re-pair or an epoch move is seen
 * by both at once. HubStore itself stays package-private.
 */
public final class LinkTrust {
    private LinkTrust() { }

    public static TrustStore open(Context ctx) {
        return new HubStore(ctx);
    }
}
