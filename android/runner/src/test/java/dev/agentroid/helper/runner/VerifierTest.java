package dev.agentroid.helper.runner;

import dev.agentroid.helper.core.Json;

import java.util.Arrays;
import java.util.List;
import java.util.Map;

/** Expired, stale-epoch, unsigned and otherwise unacceptable segments are refused and nothing runs. */
public final class VerifierTest {
    private static List<Map<String, Object>> steps() {
        return Arrays.asList(TestHub.read("s1"), TestHub.tap("s2"));
    }

    private static void refused(Rig r, String json, String code) throws Exception {
        r.run(json, 60_000);
        A.eq("refused", r.done.outcome, code);
        A.eq(0, r.exec.acts.size(), code + ": nothing acted");
        A.eq(0, r.exec.queries, code + ": the screen was not even read");
        A.ok(r.journal.count("refused") >= 1, code + ": the refusal is journalled");
        String rec = String.valueOf(r.journal.records.get(0).get("reason"));
        A.ok(rec.startsWith(code), code + ": journal says " + rec);
        A.eq(0, r.trust.writes, "the runner never writes the link's trust store");
    }

    public void testStaleEpochIsRefused() throws Exception {
        Rig r = new Rig(true, false, 0);
        String json = r.signed("seg", "R1", steps());
        r.trust.epoch = Rig.EPOCH + 1; // a newer hub has since reached ready
        refused(r, json, "stale_epoch");
        A.eq("stale_epoch", r.report().get("reason"), "report reason");
    }

    public void testEpochAheadIsRefused() throws Exception {
        Rig r = new Rig(true, false, 0);
        r.hub.epoch = Rig.EPOCH + 1;
        refused(r, r.signed("seg", "R1", steps()), "epoch_ahead");
    }

    public void testExpiredIsRefused() throws Exception {
        Rig r = new Rig(true, false, 0);
        String json = r.hub.sign(r.hub.segment("seg", "R1", Rig.NOW, false, steps())); // expires exactly now
        refused(r, json, "expired");
        Rig r2 = new Rig(true, false, 0);
        String json2 = r2.signed("seg", "R1", steps());
        r2.clock.advance(31 * 60_000L);
        refused(r2, json2, "expired");
    }

    public void testUnsignedIsRefused() throws Exception {
        Rig r = new Rig(true, false, 0);
        Map<String, Object> env = (Map<String, Object>) Json.parse(r.signed("seg", "R1", steps()));
        env.remove("signature");
        refused(r, Json.write(env), "unsigned");
        Rig r2 = new Rig(true, false, 0);
        Map<String, Object> env2 = (Map<String, Object>) Json.parse(r2.signed("seg", "R1", steps()));
        ((Map<String, Object>) env2.get("signature")).put("value", "");
        refused(r2, Json.write(env2), "unsigned");
    }

    public void testSignedByAnotherKeyIsRefused() throws Exception {
        Rig r = new Rig(true, false, 0);
        TestHub impostor = new TestHub(Rig.SERIAL, Rig.EPOCH);
        refused(r, impostor.sign(impostor.segment("seg", "R1", Rig.NOW + 600_000, false, steps())), "wrong_hub_key");
    }

    public void testWrongDeviceIsRefused() throws Exception {
        Rig r = new Rig(true, false, 0);
        TestHub other = r.hub;
        Map<String, Object> seg = other.segment("seg", "R1", Rig.NOW + 600_000, false, steps());
        seg.put("device", "another-phone");
        refused(r, other.sign(seg), "wrong_device");
    }

    public void testTooLongALifeIsRefused() throws Exception {
        Rig r = new Rig(true, false, 0);
        refused(r, r.hub.sign(r.hub.segment("seg", "R1", Rig.NOW + (Segment.MAX_TTL_S + Segment.CLOCK_SKEW_S + 1) * 1000, false, steps())), "invalid");
    }

    public void testAClassificationAboveR1IsRefused() throws Exception {
        Rig r = new Rig(true, false, 0);
        refused(r, r.hub.sign(r.hub.segment("seg", "R2", Rig.NOW + 600_000, false, steps())), "classification_not_r0_r1");
    }

    public void testGarbageAndUnknownFieldsAreRefused() throws Exception {
        Rig r = new Rig(true, false, 0);
        refused(r, "{not json", "malformed");
        Rig r2 = new Rig(true, false, 0);
        Map<String, Object> seg = r2.hub.segment("seg", "R1", Rig.NOW + 600_000, false, steps());
        ((Map<String, Object>) ((List<Object>) seg.get("steps")).get(0)).put("coords", "1,2");
        refused(r2, r2.hub.sign(seg), "invalid");
    }
}
