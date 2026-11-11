package dev.agentroid.helper.link.net;

import dev.agentroid.helper.core.Challenge;

/**
 * Replaces the 2.5 StubChallenge in the app. Under 2.12 authentication happens on the link layer (LinkClient's
 * challenge), and requests reach the Dispatcher only after the hub's ready. The 2.5 `auth.challenge` RPC is kept for
 * compatibility but can no longer grant anything: it passes only when the link layer is already READY (so calling it
 * changes nothing) and fails otherwise. A stub pass cannot be mistaken for real auth any more.
 */
public final class LinkChallenge implements Challenge {
    public interface ReadyCheck { boolean linkReady(); }

    private final ReadyCheck ready;

    public LinkChallenge(ReadyCheck ready) { this.ready = ready; }

    @Override
    public Outcome answer(String nonceHex) {
        return ready.linkReady() ? Outcome.pass(null) : Outcome.fail("auth.challenge is replaced by the link-layer challenge (link.json)");
    }

    @Override
    public String algorithm() { return Identity.ALG; }
}
