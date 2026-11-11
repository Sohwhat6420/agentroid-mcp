package dev.agentroid.helper.runner;

import java.io.File;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.Map;

/**
 * Per-step overhead of the runner itself, measured in the JVM (NOT on a phone): wall time spent running a 500-step
 * segment whose executor answers instantly, divided by 500. Verification (one signature check per segment) is timed
 * separately. Labelled JVM; the phone figure is due on the POCO.
 */
public final class OverheadTest {
    static final int STEPS = 500;

    static List<Map<String, Object>> taps() {
        List<Map<String, Object>> s = new ArrayList<Map<String, Object>>();
        for (int i = 1; i <= STEPS; i++) s.add(i % 2 == 0 ? TestHub.tap("s" + i) : TestHub.read("s" + i));
        return s;
    }

    /** Median per-step ms over 5 measured runs after 3 warm-ups. */
    static double[] measure(boolean fileJournal) throws Exception {
        double[] perStep = new double[5];
        double[] verify = new double[5];
        for (int run = -3; run < 5; run++) {
            File f = null;
            FileJournal fj = null;
            if (fileJournal) {
                f = A.out().resolve("journal-overhead-" + run + ".jsonl").toFile();
                f.delete();
                fj = new FileJournal(f);
            }
            Rig r = new Rig(true, false, 0, fj);
            String json = r.signed("seg-overhead", "R1", taps());
            long v0 = System.nanoTime();
            r.verifier.verify(json);
            long v1 = System.nanoTime();
            long t0 = System.nanoTime();
            r.runner.submit(json);
            r.clock.runIdle(60 * 60_000L);
            long t1 = System.nanoTime();
            A.eq("completed", r.done.outcome, "overhead run");
            if (fj != null) { fj.close(); f.delete(); }
            if (run >= 0) {
                perStep[run] = (t1 - t0 - (v1 - v0)) / 1e6 / STEPS; // submit verifies again; take that out
                verify[run] = (v1 - v0) / 1e6;
            }
        }
        Arrays.sort(perStep);
        Arrays.sort(verify);
        return new double[] {perStep[2], perStep[4], verify[2]};
    }

    public void testPerStepOverheadInTheJvmIsUnder30Ms() throws Exception {
        double[] mem = measure(false);
        double[] file = measure(true);
        System.out.println(String.format("  JVM per-step overhead (not phone): in-memory journal median %.4f ms (worst of 5 %.4f); fsynced file journal median %.4f ms (worst %.4f); signature+checks per segment %.2f ms",
                mem[0], mem[1], file[0], file[1], mem[2]));
        A.ok(mem[1] < 30.0, "in-memory journal per-step " + mem[1] + " ms");
        A.ok(file[1] < 30.0, "fsynced journal per-step " + file[1] + " ms");
    }
}
