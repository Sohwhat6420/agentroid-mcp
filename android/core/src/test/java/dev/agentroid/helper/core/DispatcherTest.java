package dev.agentroid.helper.core;

import java.util.Map;

/** Request routing, strict params, id deduplication. */
public class DispatcherTest {

    public void testRoutingReachesTheRightPort() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        T.ok(T.obj(rig.req(1, "tap", "{\"x\":10,\"y\":20.5}")).containsKey("result"), "tap answered");
        T.ok(T.obj(rig.req(2, "swipe", "{\"x1\":1,\"y1\":2,\"x2\":3,\"y2\":4,\"ms\":250}")).containsKey("result"), "swipe answered");
        T.ok(T.obj(rig.req(3, "swipe", "{\"x1\":1,\"y1\":2,\"x2\":3,\"y2\":4}")).containsKey("result"), "swipe default ms answered");
        T.ok(T.obj(rig.req(4, "set_value", "{\"id\":\"com.example:id/seek\",\"value\":35}")).containsKey("result"), "set_value answered");
        T.ok(T.obj(rig.req(5, "tree", null)).containsKey("result"), "tree answered");
        T.eq(Long.valueOf(0), T.at(rig.req(6, "tree", null), "result", "nodes"), "empty tree has 0 nodes");
        Object shot = rig.req(7, "screenshot", null);
        T.eq("AQID", T.at(shot, "result", "png_b64"), "screenshot bytes 01 02 03 -> transport text");
        T.eq(Long.valueOf(2), T.at(shot, "result", "width"), "screenshot width");
        T.eq("png", T.at(shot, "result", "format"), "screenshot format");

        T.eq("tap 10.0 20.5", rig.a11y.calls.get(0), "tap call");
        T.eq("swipe 1.0 2.0 3.0 4.0 250", rig.a11y.calls.get(1), "swipe call with ms");
        T.eq("swipe 1.0 2.0 3.0 4.0 300", rig.a11y.calls.get(2), "swipe default 300 ms");
        T.eq("set_value id com.example:id/seek 35.0", rig.a11y.calls.get(3), "set_value is ONE port call carrying selector and value");
        T.eq(Integer.valueOf(7), Integer.valueOf(rig.a11y.calls.size()), "no extra port calls");
    }

    public void testSetValueGoesThroughOnePortCall() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        Object r = rig.req("sv", "set_value", "{\"desc\":\"Volume\",\"value\":0.5}");
        T.eq(Double.valueOf(0.5), T.at(r, "result", "now"), "value read back is returned");
        T.eq(Integer.valueOf(1), Integer.valueOf(rig.a11y.calls.size()), "exactly one call");
        T.eq("set_value desc Volume 0.5", rig.a11y.calls.get(0), "call content");
    }

    public void testUnknownMethodAndParamErrors() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        T.eq(Long.valueOf(ErrorCodes.METHOD_NOT_FOUND), Long.valueOf(Fakes.errCode(rig.req(1, "reboot", null))), "unknown method");
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(2, "tap", "{\"x\":1}"))), "missing y");
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(3, "tap", "{\"x\":\"1\",\"y\":2}"))), "string x");
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(4, "tap", "{\"x\":1,\"y\":2,\"z\":3}"))), "unknown param");
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(5, "tap", "{\"x\":-1,\"y\":2}"))), "negative x");
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(6, "tap", "{\"x\":1e300,\"y\":2}"))), "huge x");
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(7, "swipe", "{\"x1\":1,\"y1\":2,\"x2\":3,\"y2\":4,\"ms\":0}"))), "ms 0");
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(8, "swipe", "{\"x1\":1,\"y1\":2,\"x2\":3,\"y2\":4,\"ms\":1.5}"))), "ms fractional");
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(9, "set_value", "{\"id\":\"a\",\"text\":\"b\",\"value\":1}"))), "two selectors");
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(10, "set_value", "{\"value\":1}"))), "no selector");
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(11, "set_value", "{\"id\":\"\",\"value\":1}"))), "empty selector");
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(12, "set_value", "{\"id\":\"a\"}"))), "no value");
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(13, "tree", "{\"deep\":true}"))), "tree takes no params");
        T.ok(rig.a11y.calls.isEmpty(), "no invalid request reached the phone");
    }

    public void testIdsAreEchoedExactly() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        T.eq(Long.valueOf(123), T.obj(rig.req(123, "ping", null)).get("id"), "integer id");
        T.eq("req-9", T.obj(rig.req("req-9", "ping", null)).get("id"), "string id");
        T.eq(Long.valueOf(5), T.obj(rig.req(5, "nosuch", null)).get("id"), "id on error");
    }

    public void testResentActRequestIsAnsweredFromMemoryNotRepeated() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        Object a = rig.req("dup", "tap", "{\"x\":5,\"y\":6}");
        Object b = rig.req("dup", "tap", "{\"x\":5,\"y\":6}");
        T.eq(Json.write(a), Json.write(b), "same answer");
        T.eq(Integer.valueOf(1), Integer.valueOf(rig.a11y.calls.size()), "tap performed once");
        Object c = rig.req("dup", "tap", "{\"x\":9,\"y\":9}");
        T.eq(Long.valueOf(ErrorCodes.INVALID_REQUEST), Long.valueOf(Fakes.errCode(c)), "id reused for a different request is refused");
        T.eq(Integer.valueOf(1), Integer.valueOf(rig.a11y.calls.size()), "and not performed");
        // integer 1 and string "1" are different ids
        rig.req(1, "tap", "{\"x\":1,\"y\":1}");
        rig.req("1", "tap", "{\"x\":1,\"y\":1}");
        T.eq(Integer.valueOf(3), Integer.valueOf(rig.a11y.calls.size()), "1 and \"1\" are separate ids");
    }

    public void testATransientActAnswerIsNotRememberedSoTheSameIdMayRunAgain() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        final int[] calls = {0};
        rig.dispatcher.register("flaky", Dispatcher.Kind.ACT, p -> {
            if (++calls[0] == 1) throw new HandlerException(ErrorCodes.TRANSIENT, "busy", null);
            return "ok";
        });
        Object first = rig.req("flaky-1", "flaky", "{}");
        T.eq(Long.valueOf(ErrorCodes.TRANSIENT), Long.valueOf(Fakes.errCode(first)), "told to try again");
        Object second = rig.req("flaky-1", "flaky", "{}");
        T.eq(Integer.valueOf(2), Integer.valueOf(calls[0]), "the same id reached the handler again");
        T.eq("ok", T.at(second, "result"), "and got the real answer");
        // an ordinary error IS remembered, as before
        rig.dispatcher.register("fails", Dispatcher.Kind.ACT, p -> { calls[0]++; throw new HandlerException(ErrorCodes.DEVICE_ERROR, "no", null); });
        rig.req("f-1", "fails", "{}");
        int before = calls[0];
        rig.req("f-1", "fails", "{}");
        T.eq(Integer.valueOf(before), Integer.valueOf(calls[0]), "a device error is answered from memory");
    }

    public void testHandlerCrashBecomesInternalErrorWithoutDetails() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        rig.a11y.boom = new IllegalStateException("secret screen text 4111 1111");
        Object r = rig.req(1, "tap", "{\"x\":1,\"y\":1}");
        T.eq(Long.valueOf(ErrorCodes.INTERNAL_ERROR), Long.valueOf(Fakes.errCode(r)), "internal error code");
        T.ok(!Json.write(r).contains("4111"), "exception text is not leaked to the link");
    }

    public void testPortFailureIsADeviceErrorWithReason() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        rig.ports.setA11y(null);
        Object r = rig.req(1, "tap", "{\"x\":1,\"y\":1}");
        T.eq(Long.valueOf(ErrorCodes.DEVICE_ERROR), Long.valueOf(Fakes.errCode(r)), "device error");
        T.eq("a11y_unbound", T.at(r, "error", "data", "reason"), "reason");
    }

    public void testEveryMethodHasADeclaredKindAndNoneIsRegisteredTwice() {
        Fakes.Rig rig = new Fakes.Rig();
        T.throwsA(IllegalStateException.class, () -> rig.dispatcher.register("tap", Dispatcher.Kind.ACT, p -> null), "double register");
        T.throwsA(IllegalArgumentException.class, () -> rig.dispatcher.register("x", null, p -> null), "kind is mandatory");
        T.eq(Integer.valueOf(9), Integer.valueOf(HelperMethods.methodNames().size()), "nine methods in the table");
    }
}
