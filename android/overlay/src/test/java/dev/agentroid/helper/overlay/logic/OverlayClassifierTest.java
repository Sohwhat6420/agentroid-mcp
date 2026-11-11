package dev.agentroid.helper.overlay.logic;

import static dev.agentroid.helper.overlay.logic.OverlayClassifier.ACTION_DOWN;
import static dev.agentroid.helper.overlay.logic.OverlayClassifier.ACTION_OUTSIDE;
import static dev.agentroid.helper.overlay.logic.OverlayClassifier.FLAG_IS_ACCESSIBILITY_EVENT;
import static dev.agentroid.helper.overlay.logic.OverlayClassifier.FLAG_IS_GENERATED_GESTURE;
import static dev.agentroid.helper.overlay.logic.OverlayTestRunner.eq;
import static dev.agentroid.helper.overlay.logic.OverlayTestRunner.ok;

import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Random;

import dev.agentroid.helper.overlay.logic.OverlayClassifier.Verdict;

/**
 * Synthetic touches only (no capture, no personal data). The touchscreen is device 3; the other devices are the ones a
 * phone with scrcpy attached would list.
 */
public class OverlayClassifierTest {
    static final int TS = 3;
    static final int SRC_TOUCH = 0x1002, SRC_STYLUS_ON_TOUCH = 0x5002, SRC_MOUSE = 0x2002, SRC_KEYBOARD = 0x101;
    static final int TOOL_FINGER = 1, TOOL_STYLUS = 2;
    static final int ACTION_MOVE = 2, ACTION_UP = 1;

    static TouchscreenRegistry registry() {
        TouchscreenRegistry r = new TouchscreenRegistry();
        r.seed(Arrays.asList(
                new TouchscreenRegistry.Device(-1, "Virtual", SRC_KEYBOARD, true, false),
                new TouchscreenRegistry.Device(2, "synthetic-gpio-keys", SRC_KEYBOARD, false, false),
                new TouchscreenRegistry.Device(TS, "synthetic-touchscreen", SRC_STYLUS_ON_TOUCH, false, false),
                new TouchscreenRegistry.Device(4, "uinput-touch-injector", SRC_TOUCH, false, false),
                new TouchscreenRegistry.Device(5, "hid-generic-panel", SRC_TOUCH, false, true)));
        // scrcpy --keyboard=uhid --mouse=uhid starts after the service, and a look-alike on an internal bus
        r.onAdded(new TouchscreenRegistry.Device(9, "scrcpy", SRC_TOUCH | SRC_MOUSE | SRC_KEYBOARD, false, false));
        r.onAdded(new TouchscreenRegistry.Device(10, "panel-0001", SRC_TOUCH, false, false));
        return r;
    }

    /** 20 synthetic human touches on the touchscreen: outside taps, taps on the dot, stylus contact. */
    static List<TouchSample> humanTouches() {
        List<TouchSample> out = new ArrayList<TouchSample>();
        long t = 100_000;
        for (int i = 0; i < 20; i++) {
            t += 2_000 + i * 137;
            boolean pen = i % 7 == 3;
            int action = i % 9 == 4 ? ACTION_DOWN : ACTION_OUTSIDE;
            out.add(new TouchSample(action, pen ? SRC_STYLUS_ON_TOUCH : SRC_TOUCH, TS, 0, pen ? TOOL_STYLUS : TOOL_FINGER, t));
        }
        return out;
    }

    public void testRegistryKeepsOnlyThePhysicalTouchscreen() {
        TouchscreenRegistry r = registry();
        ok(r.qualifies(TS), "touchscreen qualifies");
        for (int id : new int[] {-1, 0, 2, 4, 5, 9, 10, 77}) ok(!r.qualifies(id), "device " + id + " must not qualify");
        eq("name says uinput", r.reason(4), "uinput");
        eq("external device", r.reason(5), "external");
        eq("added while running (hot-plug)", r.reason(9), "scrcpy uhid");
        eq("added while running (hot-plug)", r.reason(10), "look-alike");
    }

    public void testCatchesTwentyOfTwentyHumanTouchesOnApi30Plus() {
        for (int sdk : new int[] {30, 34, 35}) {
            OverlayClassifier c = new OverlayClassifier(registry(), sdk);
            int caught = 0;
            for (TouchSample s : humanTouches()) if (c.classify(s) == Verdict.HUMAN) caught++;
            eq(20, caught, "sdk " + sdk);
        }
    }

    public void testCatchesAtLeastNineteenOfTwentyOnApi29WithOurOwnGesturesRunning() {
        OverlayClassifier c = new OverlayClassifier(registry(), 29);
        List<TouchSample> human = humanTouches();
        int caught = 0;
        for (int i = 0; i < human.size(); i++) {
            TouchSample s = human.get(i);
            // our runner dispatches a 300 ms gesture before every touch; once (touch 7) the human lands inside it
            long start = i == 7 ? s.eventTimeMs - 100 : s.eventTimeMs - 1_000;
            c.noteOwnGesture(start, 300);
            if (c.classify(s) == Verdict.HUMAN) caught++;
        }
        ok(caught >= 19, "caught " + caught + " of 20");
        eq(19, caught, "exactly the touch inside our own gesture window is missed");
    }

    /** 500 generated gestures (dispatchGesture) and UHID events: 0 false positives. */
    public void testZeroFalsePositivesOnFiveHundredGeneratedGestures() {
        Random rnd = new Random(215);
        int human = 0, n = 0;
        OverlayClassifier api34 = new OverlayClassifier(registry(), 34);
        OverlayClassifier api29 = new OverlayClassifier(registry(), 29);
        long t = 500_000;
        // 200 dispatchGesture on API 30+: FLAG_IS_ACCESSIBILITY_EVENT, sometimes also FLAG_IS_GENERATED_GESTURE,
        // whatever device id they carry (even the touchscreen's)
        for (int i = 0; i < 200; i++, n++) {
            int flags = FLAG_IS_ACCESSIBILITY_EVENT | (rnd.nextBoolean() ? FLAG_IS_GENERATED_GESTURE : 0);
            int dev = new int[] {-1, 0, TS}[i % 3];
            Verdict v = api34.classify(new TouchSample(i % 2 == 0 ? ACTION_OUTSIDE : ACTION_DOWN, SRC_TOUCH, dev, flags, TOOL_FINGER, t += 300));
            if (v == Verdict.HUMAN) human++;
            ok(v == Verdict.ACCESSIBILITY_INJECTED, "api34 gesture " + i + ": " + v);
        }
        // 100 dispatchGesture on API 29 with FLAG_IS_GENERATED_GESTURE
        for (int i = 0; i < 100; i++, n++) {
            Verdict v = api29.classify(new TouchSample(ACTION_OUTSIDE, SRC_TOUCH, i % 2 == 0 ? 0 : -1, FLAG_IS_GENERATED_GESTURE, TOOL_FINGER, t += 300));
            if (v == Verdict.HUMAN) human++;
        }
        // 100 dispatchGesture on API 29 with neither flag: device id 0 or -1
        for (int i = 0; i < 100; i++, n++) {
            Verdict v = api29.classify(new TouchSample(ACTION_OUTSIDE, SRC_TOUCH, i % 2 == 0 ? 0 : -1, 0, TOOL_FINGER, t += 300));
            if (v == Verdict.HUMAN) human++;
            eq(Verdict.NO_DEVICE, v, "api29 flagless gesture");
        }
        // 50 worst-case API 29 gestures that carry the touchscreen's id and no flag: caught by our own gesture window
        for (int i = 0; i < 50; i++, n++) {
            long start = t += 1_000;
            api29.noteOwnGesture(start, 200 + rnd.nextInt(400));
            Verdict v = api29.classify(new TouchSample(ACTION_OUTSIDE, SRC_TOUCH, TS, 0, TOOL_FINGER, start + rnd.nextInt(200)));
            if (v == Verdict.HUMAN) human++;
            eq(Verdict.OWN_GESTURE_WINDOW, v, "api29 own window");
        }
        // 50 scrcpy UHID and other virtual-device events
        int[] virt = {9, 10, 4, 5, -1};
        for (int i = 0; i < 50; i++, n++) {
            Verdict v = api34.classify(new TouchSample(ACTION_OUTSIDE, SRC_TOUCH, virt[i % virt.length], 0, TOOL_FINGER, t += 300));
            if (v == Verdict.HUMAN) human++;
        }
        eq(500, n, "actions");
        eq(0, human, "false positives");
    }

    public void testScrcpyInjectAndInputWithDeviceIdZeroAreRejected() {
        OverlayClassifier c = new OverlayClassifier(registry(), 34);
        for (int i = 0; i < 100; i++) eq(Verdict.NO_DEVICE, c.classify(new TouchSample(ACTION_OUTSIDE, SRC_TOUCH, 0, 0, TOOL_FINGER, 1_000 + i)), "id 0");
    }

    /** Documented limitation: an {@code input tap} that carries the touchscreen's own id looks exactly like a finger. */
    public void testInputTapWithTheTouchscreensIdIsNotDistinguishableOnThePhone() {
        OverlayClassifier c = new OverlayClassifier(registry(), 34);
        eq(Verdict.HUMAN, c.classify(new TouchSample(ACTION_OUTSIDE, SRC_TOUCH, TS, 0, TOOL_FINGER, 5_000)),
                "needs adb, so it cannot happen with adb down; with adb up the hub filters it by its own injection log");
    }

    public void testNonTouchEventsNeverCount() {
        OverlayClassifier c = new OverlayClassifier(registry(), 34);
        eq(Verdict.NOT_TOUCHSCREEN_SOURCE, c.classify(new TouchSample(ACTION_OUTSIDE, SRC_MOUSE, TS, 0, 3, 1)), "mouse");
        eq(Verdict.NOT_TOUCHSCREEN_SOURCE, c.classify(new TouchSample(ACTION_DOWN, SRC_KEYBOARD, 2, 0, 0, 2)), "keys");
        eq(Verdict.NOT_A_DOWN, c.classify(new TouchSample(ACTION_MOVE, SRC_TOUCH, TS, 0, TOOL_FINGER, 3)), "move");
        eq(Verdict.NOT_A_DOWN, c.classify(new TouchSample(ACTION_UP, SRC_TOUCH, TS, 0, TOOL_FINGER, 4)), "up");
    }

    public void testHumanTouchesDuringOurGestureStillCountOnApi30Plus() {
        OverlayClassifier c = new OverlayClassifier(registry(), 34);
        c.noteOwnGesture(1_000, 500);
        eq(Verdict.HUMAN, c.classify(new TouchSample(ACTION_OUTSIDE, SRC_TOUCH, TS, 0, TOOL_FINGER, 1_200)), "flags are conclusive on 30+");
    }
}
