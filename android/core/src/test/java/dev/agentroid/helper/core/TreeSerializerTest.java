package dev.agentroid.helper.core;

import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/** Node tree to schema JSON, checked against the shape validator after a real write/parse round trip. */
public class TreeSerializerTest {

    private static Fakes.Window sampleWindow() {
        Fakes.Node seek = new Fakes.Node().cls("android.widget.SeekBar").id("com.example:id/seek");
        seek.range = new double[] {0, 100, 30};
        seek.focusable = true;
        Fakes.Node pw = new Fakes.Node().cls("android.widget.EditText").id("com.example:id/pw").text("hunter2-secret");
        pw.password = true;
        pw.editable = true;
        Fakes.Node label = new Fakes.Node().cls("android.widget.TextView").text("café 東京").desc("greeting").click();
        label.important = false;
        Fakes.Node ghost = new Fakes.Node().cls("android.view.View"); // child that is a normal node
        Fakes.Node root = new Fakes.Node().cls("android.widget.FrameLayout").id("com.example:id/root")
                .add(label).add(pw).add(seek).add(new Fakes.Node().add(ghost));
        root.nullChild = true; // one child vanished mid-walk
        return new Fakes.Window(root);
    }

    private static Object roundTrip(Object tree) throws Exception {
        byte[] wire = Json.write(tree).getBytes(StandardCharsets.UTF_8);
        return Json.parse(Frames.decodeUtf8Strict(wire), TreeSerializer.MAX_JSON_NESTING);
    }

    /** Deepest bracket nesting of a JSON text, ignoring brackets inside strings. */
    private static int nesting(String json) {
        int depth = 0, max = 0;
        boolean inStr = false;
        for (int i = 0; i < json.length(); i++) {
            char c = json.charAt(i);
            if (inStr) {
                if (c == '\\') i++;
                else if (c == '"') inStr = false;
            } else if (c == '"') inStr = true;
            else if (c == '{' || c == '[') max = Math.max(max, ++depth);
            else if (c == '}' || c == ']') depth--;
        }
        return max;
    }

    public void testDeepestTreeStaysInsideTheHubParsersNestingLimit() {
        Fakes.Node top = new Fakes.Node();
        top.range = new double[] {0, 1, 0};   // the deepest leaf also carries a nested range object
        for (int i = 0; i < 200; i++) top = new Fakes.Node().add(top);
        String json = Json.write(TreeSerializer.serialize(Arrays.asList(new Fakes.Window(top))));
        int n = nesting(json);
        T.ok(n <= TreeSerializer.MAX_JSON_NESTING, "JSON nesting " + n + " must be at most " + TreeSerializer.MAX_JSON_NESTING);
        T.ok(n >= 120, "the test really did reach the limit (nesting " + n + ")");
    }

    public void testSerialisedTreeMatchesTheShape() throws Exception {
        Fakes.Node.released = 0;
        Object tree = roundTrip(TreeSerializer.serialize(Arrays.asList(sampleWindow())));
        List<String> errs = ShapeCheck.validate(tree);
        T.ok(errs.isEmpty(), "shape errors: " + errs);
        T.eq(Long.valueOf(6), T.at(tree, "nodes"), "root + label + pw + seek + wrapper + ghost");
        T.eq(Long.valueOf(1), T.at(tree, "clickable"), "one clickable");
        T.eq(Boolean.FALSE, T.at(tree, "truncated"), "not truncated");
        Object root = T.at(T.arr(T.at(tree, "windows")).get(0), "root");
        T.eq("com.example:id/root", T.at(root, "id"), "view id kept");
        T.eq(Integer.valueOf(4), Integer.valueOf(T.arr(T.at(root, "children")).size()), "null child skipped, 4 real children");
        Object label = T.arr(T.at(root, "children")).get(0);
        T.eq("café 東京", T.at(label, "text"), "unicode text survives");
        T.eq(Boolean.FALSE, T.at(label, "important"), "not-important views are reported with the flag");
        Object seek = T.arr(T.at(root, "children")).get(2);
        T.eq(Double.valueOf(30), Double.valueOf(((Number) T.at(seek, "range", "current")).doubleValue()), "range current");
        T.eq(Double.valueOf(100), Double.valueOf(((Number) T.at(seek, "range", "max")).doubleValue()), "range max");
        T.eq(Integer.valueOf(6), Integer.valueOf(Fakes.Node.released), "every node released once");
    }

    public void testPasswordTextNeverLeavesThePhone() throws Exception {
        String json = Json.write(TreeSerializer.serialize(Arrays.asList(sampleWindow())));
        T.ok(!json.contains("hunter2"), "password text must not be serialised");
        Object tree = Json.parse(json);
        Object pw = T.arr(T.at(T.arr(T.at(tree, "windows")).get(0), "root", "children")).get(1);
        T.eq(Boolean.TRUE, T.at(pw, "password"), "flagged as password");
        T.ok(!T.obj(pw).containsKey("text"), "no text key");
    }

    public void testValidatorCatchesBadShapes() {
        // The validator must be able to fail, otherwise the passing test above proves nothing.
        T.ok(!ShapeCheck.validate("x").isEmpty(), "non-object");
        java.util.Map<String, Object> bad = new java.util.LinkedHashMap<String, Object>();
        bad.put("windows", new ArrayList<Object>());
        bad.put("nodes", "three");
        T.ok(!ShapeCheck.validate(bad).isEmpty(), "bad types and missing keys");
        java.util.Map<String, Object> extra = TreeSerializer.serialize(new ArrayList<UiWindow>());
        extra.put("surprise", Boolean.TRUE);
        T.ok(!ShapeCheck.validate(extra).isEmpty(), "unknown key");
    }

    public void testDepthLimitTruncatesInsteadOfOverflowing() throws Exception {
        Fakes.Node leaf = new Fakes.Node();
        Fakes.Node top = leaf;
        for (int i = 0; i < 120; i++) top = new Fakes.Node().add(top);
        Object tree = roundTrip(TreeSerializer.serialize(Arrays.asList(new Fakes.Window(top))));
        T.eq(Boolean.TRUE, T.at(tree, "truncated"), "truncated flag");
        List<String> errs = ShapeCheck.validate(tree);
        T.ok(errs.isEmpty(), "truncated tree still validates: " + errs);
        T.eq(Long.valueOf(TreeSerializer.MAX_DEPTH + 1), T.at(tree, "nodes"), "stops at the depth limit");
    }

    public void testNodeBudgetTruncates() throws Exception {
        Fakes.Node root = new Fakes.Node();
        for (int i = 0; i < TreeSerializer.MAX_NODES + 500; i++) root.add(new Fakes.Node());
        Object tree = roundTrip(TreeSerializer.serialize(Arrays.asList(new Fakes.Window(root))));
        T.eq(Boolean.TRUE, T.at(tree, "truncated"), "truncated flag");
        T.eq(Long.valueOf(TreeSerializer.MAX_NODES), T.at(tree, "nodes"), "capped at the node budget");
    }

    public void testWindowWithoutRootIsDropped() throws Exception {
        Object tree = roundTrip(TreeSerializer.serialize(Arrays.asList(new Fakes.Window(null), sampleWindow())));
        T.eq(Integer.valueOf(1), Integer.valueOf(T.arr(T.at(tree, "windows")).size()), "only the readable window");
    }

    public void testLoneSurrogateInScreenTextIsReplacedNotCorrupted() throws Exception {
        Fakes.Node n = new Fakes.Node().text("a\ud83dz"); // lone high surrogate, as some apps produce
        Object tree = roundTrip(TreeSerializer.serialize(Arrays.asList(new Fakes.Window(n))));
        Object root = T.at(T.arr(T.at(tree, "windows")).get(0), "root");
        T.eq("a�z", T.at(root, "text"), "replaced with U+FFFD");
    }
}
