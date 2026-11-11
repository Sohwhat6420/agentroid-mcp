package dev.agentroid.helper.core;

import java.util.ArrayList;
import java.util.List;

/** The auth stub, needs_repair, and the refusal of every non-status request. */
public class AuthGateTest {

    private static final String[][] ACTING = {
        {"tap", "{\"x\":1,\"y\":2}"},
        {"swipe", "{\"x1\":1,\"y1\":2,\"x2\":3,\"y2\":4}"},
        {"set_value", "{\"id\":\"a:id/b\",\"value\":3}"},
        {"type_text", "{\"text\":\"hi\"}"},
        {"tree", null},
        {"screenshot", null},
    };

    private static String auth(String id) {
        return "{\"jsonrpc\":\"2.0\",\"id\":\"" + id + "\",\"method\":\"auth.challenge\",\"params\":{\"nonce\":\"" + Fakes.NONCE + "\"}}";
    }

    private static void assertAllRefused(Fakes.Rig rig, int code, String when) {
        int i = 100;
        for (String[] m : ACTING) {
            Object r = rig.req(i++, m[0], m[1]);
            T.eq(Long.valueOf(code), Long.valueOf(Fakes.errCode(r)), when + ": " + m[0] + " refused");
        }
        T.ok(rig.a11y.calls.isEmpty(), when + ": nothing reached the accessibility port: " + rig.a11y.calls);
        T.ok(rig.ime.committed.isEmpty(), when + ": nothing reached the IME");
    }

    public void testStubPassesByDefault() {
        Fakes.Rig rig = new Fakes.Rig();
        Object r = rig.call(auth("a1"));
        T.eq("ready", T.at(r, "result", "state"), "state after a passing challenge");
        T.eq("stub", T.at(r, "result", "alg"), "the hub is told this was a stub, not real auth");
        T.ok(!T.obj(T.obj(r).get("result")).containsKey("signature"), "stub gives no signature");
    }

    public void testBeforeAnyChallengeActingIsRefusedAndStatusAnswers() {
        Fakes.Rig rig = new Fakes.Rig();
        assertAllRefused(rig, ErrorCodes.AUTH_REQUIRED, "pending_auth");
        T.eq("pending_auth", T.at(rig.req(1, "status", null), "result", "link", "state"), "status answers");
        T.eq(Boolean.TRUE, T.at(rig.req(2, "ping", null), "result", "pong"), "ping answers");
    }

    public void testForcedFailureGoesToNeedsRepairAndRefusesEveryActingRequest() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        T.ok(T.obj(rig.req("ok1", "tap", "{\"x\":1,\"y\":1}")).containsKey("result"), "acts while ready");
        rig.a11y.calls.clear();

        rig.challenge.forceFail(true);
        Object r = rig.call(auth("bad"));
        T.eq(Long.valueOf(ErrorCodes.NEEDS_REPAIR), Long.valueOf(Fakes.errCode(r)), "failed challenge answers needs_repair");
        T.eq("needs_repair", T.at(r, "error", "data", "state"), "state in error data");
        T.ok(T.at(r, "error", "data", "fix") instanceof String, "carries the fix text");
        T.eq(LinkSession.State.NEEDS_REPAIR, rig.session.state(), "session state");

        assertAllRefused(rig, ErrorCodes.NEEDS_REPAIR, "needs_repair");

        // reads of the status still answer, and say why
        Object st = rig.req("s", "status", null);
        T.eq("needs_repair", T.at(st, "result", "link", "state"), "status reports needs_repair");
        T.eq(LinkSession.REPAIR_FIX, T.at(st, "result", "link", "fix"), "status carries the fix");
        T.eq("stub", T.at(st, "result", "link", "auth"), "status names the auth kind");
        T.eq(Boolean.TRUE, T.at(rig.req("p", "ping", null), "result", "pong"), "ping still answers");
        T.ok(T.at(st, "result", "sources") != null, "status still includes the source report");
    }

    public void testRefusalIsNotCachedAsADedupedResult() {
        Fakes.Rig rig = new Fakes.Rig();
        Object refused = rig.req("same", "tap", "{\"x\":1,\"y\":1}");
        T.eq(Long.valueOf(ErrorCodes.AUTH_REQUIRED), Long.valueOf(Fakes.errCode(refused)), "refused before auth");
        rig.authed();
        Object ok = rig.req("same", "tap", "{\"x\":1,\"y\":1}");
        T.ok(T.obj(ok).containsKey("result"), "same id works after auth: " + Json.write(ok));
        T.eq(Integer.valueOf(1), Integer.valueOf(rig.a11y.calls.size()), "performed once");
    }

    public void testNeedsRepairIsStickyUntilAChallengePasses() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        rig.challenge.forceFail(true);
        rig.call(auth("bad"));
        rig.challenge.forceFail(false);                         // the fault clears, but no new challenge yet
        assertAllRefused(rig, ErrorCodes.NEEDS_REPAIR, "still needs_repair without a new challenge");
        rig.session.onDisconnect();                              // a reconnect alone does not clear it either
        T.eq(LinkSession.State.NEEDS_REPAIR, rig.session.state(), "disconnect keeps needs_repair");
        assertAllRefused(rig, ErrorCodes.NEEDS_REPAIR, "after reconnect, before a challenge");
        Object r = rig.call(auth("good"));
        T.eq("ready", T.at(r, "result", "state"), "a passing challenge recovers");
        T.ok(T.obj(rig.req("t", "tap", "{\"x\":1,\"y\":1}")).containsKey("result"), "acting works again");
    }

    public void testDisconnectFromReadyRequiresANewChallenge() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        rig.session.onDisconnect();
        T.eq(LinkSession.State.PENDING_AUTH, rig.session.state(), "back to pending_auth");
        assertAllRefused(rig, ErrorCodes.AUTH_REQUIRED, "new connection");
    }

    public void testChallengeThatCrashesCountsAsFailed() {
        Challenge crashing = new Challenge() {
            public Outcome answer(String n) { throw new IllegalStateException("keystore exploded"); }
            public String algorithm() { return "crash"; }
        };
        LinkSession s = new LinkSession(crashing);
        Challenge.Outcome o = s.authenticate(Fakes.NONCE);
        T.ok(!o.ok, "failed");
        T.eq(LinkSession.State.NEEDS_REPAIR, s.state(), "needs_repair");
        T.ok(s.gate() != null && s.gate().code == ErrorCodes.NEEDS_REPAIR, "gate refuses");
    }

    public void testBadNonceIsAParamErrorAndChangesNothing() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        List<String> bad = new ArrayList<String>();
        bad.add("\"zz\"");
        bad.add("\"" + Fakes.NONCE.toUpperCase() + "\"");
        bad.add("\"abcd\"");
        bad.add("12");
        int i = 0;
        for (String n : bad) {
            Object r = rig.req("n" + (i++), "auth.challenge", "{\"nonce\":" + n + "}");
            T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(r)), "bad nonce " + n);
        }
        T.eq(LinkSession.State.READY, rig.session.state(), "a malformed nonce does not disturb the session");
    }

    public void testHiddenNotificationBlocksActing() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        rig.session.setNotificationVisible(false);
        assertAllRefused(rig, ErrorCodes.NOTIFICATION_HIDDEN, "notification hidden");
        T.eq(Boolean.FALSE, T.at(rig.req(1, "status", null), "result", "link", "notification_visible"), "status says so");
        rig.session.setNotificationVisible(true);
        T.ok(T.obj(rig.req(2, "tap", "{\"x\":1,\"y\":1}")).containsKey("result"), "acts again when visible");
    }

    public void testRepairWinsOverOtherRefusals() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        rig.challenge.forceFail(true);
        rig.call(auth("bad"));
        rig.session.setNotificationVisible(false);
        T.eq(Long.valueOf(ErrorCodes.NEEDS_REPAIR), Long.valueOf(Fakes.errCode(rig.req(1, "tap", "{\"x\":1,\"y\":1}"))), "needs_repair named first");
    }
}
