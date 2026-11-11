package dev.agentroid.helper.overlay.logic;

import static dev.agentroid.helper.overlay.logic.OverlayTestRunner.eq;
import static dev.agentroid.helper.overlay.logic.OverlayTestRunner.ok;

import java.util.ArrayList;
import java.util.List;

public class HumanTouchLogTest {
    static final class MemStore implements HumanTouchLog.Store {
        long v = -1;
        int writes;
        @Override public void putLastTouch(long wallMs) { v = wallMs; writes++; }
        @Override public long lastTouch() { return v; }
    }

    static final class Outbox implements HumanTouchLog.Sink {
        final List<String> frames = new ArrayList<String>();
        @Override public void send(String frame) { frames.add(frame); }
    }

    public void testRecordsLocallyAndQueuesAFrame() {
        MemStore s = new MemStore();
        Outbox o = new Outbox();
        HumanTouchLog log = new HumanTouchLog(s, o);
        ok(log.record(1_700_000_000_000L), "first touch");
        eq(1_700_000_000_000L, s.v, "persisted (works with adb down)");
        eq("{\"t\":\"human_touch\",\"ts\":1700000000000}", o.frames.get(0), "frame");
        ok(log.humanTouchWithin(1_700_000_000_200L, 300), "the local flag 2.14 honours");
        ok(!log.humanTouchWithin(1_700_000_001_000L, 300), "and it ages");
    }

    public void testMergesWithinFiftyMsAndTheLatestWins() {
        MemStore s = new MemStore();
        Outbox o = new Outbox();
        HumanTouchLog log = new HumanTouchLog(s, o);
        ok(log.record(10_000), "first");
        ok(!log.record(10_050), "merged at 50 ms");
        ok(!log.record(10_010), "merged");
        eq(10_050L, log.last(), "latest wins");
        ok(log.record(10_200), "distinct");
        ok(log.record(9_000), "an older distinct touch is recorded");
        eq(10_200L, log.last(), "but never moves last back");
        eq(3, log.recorded(), "recorded");
        eq(2, log.merged(), "merged");
        eq(3, o.frames.size(), "one frame per distinct touch");
    }

    public void testSurvivesARestartThroughTheStore() {
        MemStore s = new MemStore();
        new HumanTouchLog(s, new Outbox()).record(42_000);
        HumanTouchLog again = new HumanTouchLog(s, new Outbox());
        eq(42_000L, again.last(), "reloaded");
        ok(!again.record(42_030), "and merges with the reloaded touch");
    }
}
