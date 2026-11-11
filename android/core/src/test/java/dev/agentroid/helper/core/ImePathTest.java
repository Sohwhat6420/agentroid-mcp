package dev.agentroid.helper.core;

import java.nio.charset.StandardCharsets;

/**
 * The text path link -> dispatcher -> IME port, with the committed String turned back into UTF-8 bytes and compared
 * with the expected bytes. This proves the message path is byte-exact; the real read-back on a phone is still open.
 */
public class ImePathTest {
    /** "café 東京" = c a f e-acute space Tokyo(2 kanji). */
    private static final String CAFE_TOKYO = "café 東京";
    /** 63 61 66 | c3 a9 | 20 | e6 9d b1 | e4 ba ac : written out by hand, not derived from the String. */
    private static final byte[] CAFE_TOKYO_UTF8 = {
        0x63, 0x61, 0x66, (byte) 0xc3, (byte) 0xa9, 0x20, (byte) 0xe6, (byte) 0x9d, (byte) 0xb1, (byte) 0xe4, (byte) 0xba, (byte) 0xac
    };

    private static int nextId = 1;

    /** Every frame gets its own id: a reused id with different text is (correctly) refused by the dispatcher's dedupe. */
    private static byte[] typeFrame(String jsonStringBody) {
        return ("{\"jsonrpc\":\"2.0\",\"id\":\"t" + (nextId++) + "\",\"method\":\"type_text\",\"params\":{\"text\":\"" + jsonStringBody + "\"}}")
                .getBytes(StandardCharsets.UTF_8);
    }

    public void testReusedIdWithDifferentTextIsRefusedNotTyped() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        byte[] a = ("{\"jsonrpc\":\"2.0\",\"id\":\"same\",\"method\":\"type_text\",\"params\":{\"text\":\"one\"}}").getBytes(StandardCharsets.UTF_8);
        byte[] b = ("{\"jsonrpc\":\"2.0\",\"id\":\"same\",\"method\":\"type_text\",\"params\":{\"text\":\"two\"}}").getBytes(StandardCharsets.UTF_8);
        T.ok(T.obj(rig.callBytes(a)).containsKey("result"), "first typed");
        T.eq(Long.valueOf(ErrorCodes.INVALID_REQUEST), Long.valueOf(Fakes.errCode(rig.callBytes(b))), "second refused");
        T.eq(Integer.valueOf(1), Integer.valueOf(rig.ime.committed.size()), "only the first was typed");
        T.ok(T.obj(rig.callBytes(a)).containsKey("result"), "a true resend is answered");
        T.eq(Integer.valueOf(1), Integer.valueOf(rig.ime.committed.size()), "and not typed twice");
    }

    public void testExpectedBytesConstantIsRight() {
        T.bytesEq(CAFE_TOKYO_UTF8, CAFE_TOKYO.getBytes(StandardCharsets.UTF_8), "hand-written bytes match the string");
        T.eq(Integer.valueOf(12), Integer.valueOf(CAFE_TOKYO_UTF8.length), "12 bytes");
    }

    public void testCafeTokyoRawUtf8FrameIsByteExact() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        Object r = rig.callBytes(typeFrame(CAFE_TOKYO)); // non-ASCII sent raw, as UTF-8
        T.ok(T.obj(r).containsKey("result"), "answered: " + Json.write(r));
        T.eq(Integer.valueOf(1), Integer.valueOf(rig.ime.committed.size()), "one commit");
        T.bytesEq(CAFE_TOKYO_UTF8, rig.ime.committed.get(0), "committed bytes");
        T.eq(Long.valueOf(7), T.at(r, "result", "committed_chars"), "7 characters (c a f e-acute space 2 kanji)");
        T.eq(Long.valueOf(12), T.at(r, "result", "committed_utf8_bytes"), "12 UTF-8 bytes");
        T.ok(!Json.write(r).contains("caf"), "the typed text is not echoed back");
    }

    public void testCafeTokyoEscapedFrameIsByteExact() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        // Same text sent as \\u escapes (what a JSON encoder with ensure_ascii writes, e.g. Python's default).
        rig.callBytes(typeFrame("caf\\u00e9 \\u6771\\u4eac"));
        T.bytesEq(CAFE_TOKYO_UTF8, rig.ime.committed.get(0), "committed bytes from an escaped frame");
    }

    public void testSupplementaryPlaneAndCombiningMarksAreNotAltered() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        String emoji = "😀";             // U+1F600, a surrogate pair
        String decomposed = "é";             // e + combining acute: must NOT be normalised to U+00E9
        rig.callBytes(typeFrame(emoji + decomposed));
        byte[] expect = {(byte) 0xf0, (byte) 0x9f, (byte) 0x98, (byte) 0x80, 0x65, (byte) 0xcc, (byte) 0x81};
        T.bytesEq(expect, rig.ime.committed.get(0), "emoji and decomposed e");
        rig.callBytes(typeFrame("\\ud83d\\ude00"));
        T.bytesEq(new byte[] {(byte) 0xf0, (byte) 0x9f, (byte) 0x98, (byte) 0x80}, rig.ime.committed.get(1), "emoji from escaped surrogate pair");
    }

    public void testEscapedControlCharactersAndQuotesPassThrough() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        rig.callBytes(typeFrame("line1\\nline2\\t\\\"q\\\" \\\\"));
        T.bytesEq("line1\nline2\t\"q\" \\".getBytes(StandardCharsets.UTF_8), rig.ime.committed.get(0), "escapes decoded");
    }

    public void testBrokenTextNeverReachesTheIme() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        // invalid UTF-8 inside the text value
        byte[] head = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"type_text\",\"params\":{\"text\":\"ab".getBytes(StandardCharsets.UTF_8);
        byte[] tail = "\"}}".getBytes(StandardCharsets.UTF_8);
        byte[] frame = new byte[head.length + 2 + tail.length];
        System.arraycopy(head, 0, frame, 0, head.length);
        frame[head.length] = (byte) 0xC3;
        frame[head.length + 1] = 0x28;
        System.arraycopy(tail, 0, frame, head.length + 2, tail.length);
        T.eq(Long.valueOf(ErrorCodes.PARSE_ERROR), Long.valueOf(Fakes.errCode(rig.callBytes(frame))), "invalid UTF-8 refused");
        // lone surrogate escape
        T.eq(Long.valueOf(ErrorCodes.PARSE_ERROR), Long.valueOf(Fakes.errCode(rig.callBytes(typeFrame("x\\ud83dy")))), "lone surrogate refused");
        // wrong types and empty text
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(2, "type_text", "{\"text\":5}"))), "number text");
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(3, "type_text", "{\"text\":\"\"}"))), "empty text");
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(4, "type_text", "{}"))), "missing text");
        StringBuilder big = new StringBuilder();
        for (int i = 0; i < HelperMethods.MAX_TYPE_TEXT_CHARS + 1; i++) big.append('a');
        T.eq(Long.valueOf(ErrorCodes.INVALID_PARAMS), Long.valueOf(Fakes.errCode(rig.req(5, "type_text", "{\"text\":\"" + big + "\"}"))), "text over the limit");
        T.ok(rig.ime.committed.isEmpty(), "nothing was committed by any refused request");
    }

    public void testNoActiveImeIsAReportedFailure() {
        Fakes.Rig rig = new Fakes.Rig().authed();
        rig.ports.setIme(null);
        Object r = rig.callBytes(typeFrame(CAFE_TOKYO));
        T.eq("ime_not_active", T.at(r, "error", "data", "reason"), "reason");
    }
}
