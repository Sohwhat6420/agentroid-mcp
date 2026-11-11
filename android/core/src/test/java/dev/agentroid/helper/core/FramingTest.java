package dev.agentroid.helper.core;

import java.nio.charset.StandardCharsets;
import java.util.LinkedHashMap;
import java.util.Map;

/** Framing round trip, and refusal of malformed frames. */
public class FramingTest {

    private static byte[] b(String s) { return s.getBytes(StandardCharsets.UTF_8); }

    private static FrameException refused(byte[] frame, int code, String why) {
        FrameException e = T.throwsA(FrameException.class, () -> Frames.decodeRequest(frame), why);
        T.eq(Integer.valueOf(code), Integer.valueOf(e.code), why + " (code)");
        return e;
    }

    public void testRequestRoundTrip() throws Exception {
        Frames.Request r = Frames.decodeRequest(b("{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"swipe\",\"params\":{\"x1\":1,\"y1\":2.5},\"seq\":42}"));
        T.eq(Long.valueOf(7), r.id, "integer id kept as integer");
        T.eq("swipe", r.method, "method");
        T.eq(Long.valueOf(1), r.params.get("x1"), "int param");
        T.eq(Double.valueOf(2.5), r.params.get("y1"), "double param");
        T.eq(Long.valueOf(42), r.seq, "seq");

        Frames.Request s = Frames.decodeRequest(b("{\"jsonrpc\":\"2.0\",\"id\":\"abc-1\",\"method\":\"auth.challenge\"}"));
        T.eq("abc-1", s.id, "string id kept as string");
        T.ok(s.params.isEmpty(), "absent params become an empty object");
        T.ok(s.seq == null, "absent seq is null");
    }

    public void testResponseRoundTripKeepsIdTypesAndUnicode() throws Exception {
        String text = "café 東京 😀 \"quoted\" back\\slash\nnewline";
        Map<String, Object> res = new LinkedHashMap<String, Object>();
        res.put("text", text);
        res.put("n", Long.valueOf(-5));
        res.put("d", Double.valueOf(0.25));
        res.put("nothing", null);
        for (Object id : new Object[] {Long.valueOf(99), "id-x"}) {
            byte[] wire = Frames.toBytes(Frames.result(id, res));
            Object back = Json.parse(Frames.decodeUtf8Strict(wire));
            T.eq(id, T.obj(back).get("id"), "id type survives");
            T.eq("2.0", T.obj(back).get("jsonrpc"), "version tag");
            T.eq(text, T.at(back, "result", "text"), "text survives write/UTF-8/strict-decode/parse");
            T.eq(Double.valueOf(0.25), T.at(back, "result", "d"), "double");
            T.ok(T.obj(T.obj(back).get("result")).containsKey("nothing"), "null kept");
        }
    }

    public void testErrorAndNotificationShapes() throws Exception {
        Object e = Json.parse(new String(Frames.toBytes(Frames.error("q", -32601, "nope", null)), StandardCharsets.UTF_8));
        T.eq(Long.valueOf(-32601), T.at(e, "error", "code"), "error code");
        T.ok(!T.obj(T.obj(e).get("error")).containsKey("data"), "no data key when none");
        Map<String, Object> p = new LinkedHashMap<String, Object>();
        p.put("a", Long.valueOf(1));
        Object n = Json.parse(new String(Frames.toBytes(Frames.notification("hello", p)), StandardCharsets.UTF_8));
        T.ok(!T.obj(n).containsKey("id"), "a notification has no id");
        T.eq("hello", T.obj(n).get("method"), "notification method");
    }

    public void testMalformedFramesAreRefused() throws Exception {
        refused(new byte[0], ErrorCodes.INVALID_REQUEST, "empty frame");
        refused(new byte[] {(byte) 0xC3, (byte) 0x28}, ErrorCodes.PARSE_ERROR, "invalid UTF-8 (bad continuation)");
        refused(new byte[] {(byte) 0xFF, (byte) 0xFE}, ErrorCodes.PARSE_ERROR, "invalid UTF-8 (0xFF)");
        refused(b("﻿{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}"), ErrorCodes.PARSE_ERROR, "byte-order mark");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\""), ErrorCodes.PARSE_ERROR, "truncated JSON");
        refused(b("not json"), ErrorCodes.PARSE_ERROR, "not JSON");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"} trailing"), ErrorCodes.PARSE_ERROR, "trailing text");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1,\"id\":2,\"method\":\"ping\"}"), ErrorCodes.PARSE_ERROR, "duplicate key");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"p\\ud800ing\"}"), ErrorCodes.PARSE_ERROR, "lone surrogate escape");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"params\":{\"t\":\"a\u0001b\"}}"), ErrorCodes.PARSE_ERROR, "raw control char");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":01,\"method\":\"ping\"}"), ErrorCodes.PARSE_ERROR, "leading zero number");

        refused(b("[{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}]"), ErrorCodes.INVALID_REQUEST, "batch array");
        refused(b("\"ping\""), ErrorCodes.INVALID_REQUEST, "bare string");
        refused(b("{\"jsonrpc\":\"2.0\",\"method\":\"ping\"}"), ErrorCodes.INVALID_REQUEST, "missing id");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":true,\"method\":\"ping\"}"), ErrorCodes.INVALID_REQUEST, "boolean id");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1.5,\"method\":\"ping\"}"), ErrorCodes.INVALID_REQUEST, "fractional id");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":\"\",\"method\":\"ping\"}"), ErrorCodes.INVALID_REQUEST, "empty id");
        StringBuilder longId = new StringBuilder();
        for (int i = 0; i < 129; i++) longId.append('x');
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":\"" + longId + "\",\"method\":\"ping\"}"), ErrorCodes.INVALID_REQUEST, "129-char id");
        refused(b("{\"id\":1,\"method\":\"ping\"}"), ErrorCodes.INVALID_REQUEST, "missing jsonrpc");
        refused(b("{\"jsonrpc\":\"1.0\",\"id\":1,\"method\":\"ping\"}"), ErrorCodes.INVALID_REQUEST, "wrong jsonrpc");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1}"), ErrorCodes.INVALID_REQUEST, "missing method");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"Ping\"}"), ErrorCodes.INVALID_REQUEST, "upper-case method");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"a..b\"}"), ErrorCodes.INVALID_REQUEST, "empty dotted segment");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":42}"), ErrorCodes.INVALID_REQUEST, "numeric method");
        StringBuilder longM = new StringBuilder();
        for (int i = 0; i < 65; i++) longM.append('a');
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"" + longM + "\"}"), ErrorCodes.INVALID_REQUEST, "65-char method");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"extra\":1}"), ErrorCodes.INVALID_REQUEST, "unknown top-level field");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"seq\":-1}"), ErrorCodes.INVALID_REQUEST, "negative seq");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"seq\":\"1\"}"), ErrorCodes.INVALID_REQUEST, "string seq");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"params\":[1,2]}"), ErrorCodes.INVALID_PARAMS, "array params");
        refused(b("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"params\":null}"), ErrorCodes.INVALID_PARAMS, "null params");
    }

    public void testSizeAndDepthLimits() throws Exception {
        byte[] big = new byte[Frames.MAX_REQUEST_BYTES + 1];
        java.util.Arrays.fill(big, (byte) ' ');
        refused(big, ErrorCodes.INVALID_REQUEST, "frame one byte over the limit");

        StringBuilder deep = new StringBuilder("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\",\"params\":");
        for (int i = 0; i < 70; i++) deep.append("{\"a\":");
        deep.append("1");
        for (int i = 0; i < 70; i++) deep.append("}");
        deep.append("}");
        refused(b(deep.toString()), ErrorCodes.PARSE_ERROR, "70 levels of nesting");
    }

    public void testRefusalCarriesTheIdWhenItWasReadable() throws Exception {
        FrameException e = refused(b("{\"jsonrpc\":\"2.0\",\"id\":\"keep-me\",\"method\":\"Bad\"}"), ErrorCodes.INVALID_REQUEST, "bad method");
        T.eq("keep-me", e.id, "id is carried so the refusal can be addressed");
        FrameException n = refused(b("not json"), ErrorCodes.PARSE_ERROR, "unparseable");
        T.ok(n.id == null, "no id when nothing was readable");
    }

    public void testDispatcherAnswersMalformedFramesWithoutThrowing() {
        Fakes.Rig rig = new Fakes.Rig();
        Object r1 = rig.callBytes(new byte[] {(byte) 0xC3, (byte) 0x28});
        T.eq(Long.valueOf(ErrorCodes.PARSE_ERROR), T.at(r1, "error", "code"), "bad UTF-8 answered");
        T.ok(T.obj(r1).get("id") == null && T.obj(r1).containsKey("id"), "null id on unaddressable error");
        Object r2 = rig.call("{\"jsonrpc\":\"2.0\",\"id\":\"z\",\"method\":\"BAD\"}");
        T.eq("z", T.obj(r2).get("id"), "addressed error keeps id");
        T.eq(Long.valueOf(ErrorCodes.INVALID_REQUEST), T.at(r2, "error", "code"), "invalid request answered");
        T.ok(rig.a11y.calls.isEmpty() && rig.ime.committed.isEmpty(), "nothing reached the phone");
    }
}
