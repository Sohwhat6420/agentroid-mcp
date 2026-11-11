package dev.agentroid.helper.runner;

import java.util.Arrays;
import java.util.List;
import java.util.Map;

/**
 * R2+ inside a correctly signed segment is refused on the phone: the phone re-classifies every step against the
 * bundled safe list and does not trust the hub's labels or classification.
 */
public final class LocalRefusalTest {
    public void testAnR2StepLabelledR1InsideASignedSegmentIsRefusedLocally() throws Exception {
        Rig r = new Rig(true, false, 0);
        Map<String, Object> typed = TestHub.step("s2", "type", TestHub.SETTINGS, "R1", TestHub.sel("id", "android:id/search_src_text"),
                TestHub.post("text_equals", TestHub.sel("id", "android:id/search_src_text"), "wifi"));
        typed.put("value", "wifi");
        r.run(r.signed("seg-typed", "R1", Arrays.asList(TestHub.read("s1"), typed, TestHub.tap("s3"))), 60_000);
        A.eq("refused", r.done.outcome, "outcome");
        A.eq("step_not_safe", r.report().get("reason"), "reason");
        A.eq(0, r.exec.acts.size(), "no step ran, not even the safe first one");
        List<Map<String, Object>> steps = r.reportSteps();
        A.eq(1, steps.size(), "only the offending step is listed");
        A.eq("s2", steps.get(0).get("id"), "step");
        A.eq("refused", steps.get(0).get("status"), "status");
        A.eq("blocked", steps.get(0).get("decision"), "Phase 2: require_confirmation became blocked");
        boolean journalled = false;
        for (Map<String, Object> rec : r.journal.records) {
            journalled |= "refused".equals(rec.get("status")) && "s2".equals(rec.get("step_id")) && "blocked".equals(rec.get("decision"));
        }
        A.ok(journalled, "the per-step refusal is journalled: " + r.journal.records);
    }

    public void testEveryKindOfR2IsRefusedEvenWhenTheHubSaysR0() throws Exception {
        Map<String, Object> coord = TestHub.step("x", "tap", TestHub.SETTINGS, "R0", TestHub.sel("near_text", "OK"), TestHub.post("present", TestHub.sel("text", "Done"), null));
        Map<String, Object> otherApp = TestHub.step("x", "tap", "com.example.bank", "R0", TestHub.sel("text", "Pay"), TestHub.post("present", TestHub.sel("text", "Paid"), null));
        Map<String, Object> buy = TestHub.step("x", "tap", SafeClasses.PLAY_STORE, "R0", TestHub.sel("text", "Buy"), TestHub.post("present", TestHub.sel("text", "Open"), null));
        Map<String, Object> perm = TestHub.toggle("x", "adb_enabled", "0");
        perm.put("scope", "permanent");
        perm.put("risk", "R0");
        Map<String, Object> unleased = TestHub.toggle("x", "accelerometer_rotation", "1");
        unleased.remove("lease");
        unleased.put("risk", "R0");
        Map<String, Object> push = TestHub.step("x", "push", TestHub.SETTINGS, "R0", null, TestHub.post("none", null, null));
        for (Map<String, Object> bad : Arrays.asList(coord, otherApp, buy, perm, unleased, push)) {
            Rig r = new Rig(true, true, 0);
            r.run(r.signed("seg", "R0", Arrays.asList(TestHub.read("s1"), bad)), 60_000);
            A.eq("refused", r.done.outcome, String.valueOf(bad));
            A.eq("step_not_safe", r.report().get("reason"), String.valueOf(bad));
            A.eq(0, r.exec.acts.size(), "nothing acted for " + bad);
        }
    }

    public void testPlayStoreUpdateNeedsAnOwnedPhoneOnBothSides() throws Exception {
        Map<String, Object> upd = TestHub.step("s1", "tap", SafeClasses.PLAY_STORE, "R1", TestHub.sel("text", "Update"), TestHub.post("present", TestHub.sel("text", "Open"), null));
        // signed owned, phone owned: runs
        Rig ok = new Rig(true, true, 100);
        ok.run(ok.hub.sign(ok.hub.segment("seg", "R1", Rig.NOW + 600_000, true, Arrays.asList(upd))), 60_000);
        A.eq("completed", ok.done.outcome, "owned + owned");
        // signed owned, but this phone is not the full flavour: refused locally
        Rig notMine = new Rig(true, false, 100);
        notMine.run(notMine.hub.sign(notMine.hub.segment("seg", "R1", Rig.NOW + 600_000, true, Arrays.asList(upd))), 60_000);
        A.eq("refused", notMine.done.outcome, "phone not owned");
        // phone owned, segment not: refused
        Rig notSigned = new Rig(true, true, 100);
        notSigned.run(notSigned.signed("seg", "R1", Arrays.asList(upd)), 60_000);
        A.eq("refused", notSigned.done.outcome, "segment not owned");
    }

    public void testAClassificationLowerThanItsStepsIsRefused() throws Exception {
        Rig r = new Rig(true, false, 0);
        r.run(r.signed("seg", "R0", Arrays.asList(TestHub.read("s1"), TestHub.tap("s2"))), 60_000);
        A.eq("refused", r.done.outcome, "R1 tap in a segment claimed R0");
        A.eq("classification_too_low", r.report().get("reason"), "reason");
        A.eq(0, r.exec.acts.size(), "nothing acted");
    }

    public void testRequireConfirmationIsBlockedInPhaseTwo() throws Exception {
        A.eq(Decision.BLOCKED, Decision.phaseTwo(Decision.REQUIRE_CONFIRMATION), "phase 2 rule");
        A.eq(Decision.ALLOWED, Decision.phaseTwo(Decision.ALLOWED), "allowed stays");
        Rig r = new Rig(true, false, 0);
        Map<String, Object> labelledR2 = TestHub.read("s1");
        labelledR2.put("risk", "R2"); // a listed kind, but the hub itself says R2: require_confirmation
        r.run(r.signed("seg", "R1", Arrays.asList(labelledR2)), 60_000);
        A.eq("refused", r.done.outcome, "require_confirmation does not run in Phase 2");
        A.eq("blocked", r.reportSteps().get(0).get("decision"), "reported as blocked");
        SafeClasses.StepDecision d = SafeClasses.bundled().decide(Segment.parse(r.hub.segment("q", "R1", Rig.NOW + 1000, false, Arrays.asList(labelledR2))).steps.get(0), false);
        A.eq(Decision.REQUIRE_CONFIRMATION, d.decision, "before the phase rule");
        A.eq(Decision.BLOCKED, d.finalDecision, "after it");
    }

    public void testTheBundledListIsTheHandWrittenFile() throws Exception {
        SafeClasses c = SafeClasses.bundled();
        A.eq(Risk.R0, c.listedClass("read", "anything.at.all"), "reads anywhere");
        A.eq(Risk.R1, c.listedClass("tap_unique", TestHub.SETTINGS), "unique taps in Settings");
        A.eq(Risk.R2, c.listedClass("tap_unique", SafeClasses.PLAY_STORE), "but not in the Play Store");
        A.eq(Risk.R1, c.listedClass("play_store_update", SafeClasses.PLAY_STORE), "the Update button");
        A.eq(Risk.R2, c.listedClass("play_store_update", TestHub.SETTINGS), "scoped token");
        A.eq(Risk.R1, c.listedClass("settings_toggle_leased", TestHub.SETTINGS), "leased toggles");
        A.throwsA(java.io.IOException.class, () -> SafeClasses.parse("{\"classes\":{\"R2\":{\"step_kinds\":[\"tap\"],\"packages\":[\"a\"]}}}"), "R2 class refused");
        A.throwsA(java.io.IOException.class, () -> SafeClasses.parse("{\"classes\":{\"R0\":{\"step_kinds\":[\"read\",\"read\"],\"packages\":[]}}}"), "duplicates refused");
    }
}
