package dev.agentroid.helper.core;

/** The API-34 gate for accessibilityDataSensitive, driven with fake SDK ints. */
public class ApiGateTest {

    private static final class Sink implements ApiGate.DataSensitiveSink {
        int calls;
        boolean last;
        public void setDataSensitive(boolean s) { calls++; last = s; }
    }

    public void testSdk33DoesNotSetTheFlag() {
        Sink s = new Sink();
        T.eq(Boolean.FALSE, Boolean.valueOf(ApiGate.applyDataSensitive(33, s)), "33 returns false");
        T.eq(Integer.valueOf(0), Integer.valueOf(s.calls), "sink never touched on 33");
    }

    public void testSdk34SetsTheFlag() {
        Sink s = new Sink();
        T.eq(Boolean.TRUE, Boolean.valueOf(ApiGate.applyDataSensitive(34, s)), "34 returns true");
        T.eq(Integer.valueOf(1), Integer.valueOf(s.calls), "sink called once");
        T.ok(s.last, "set to sensitive");
    }

    public void testBoundaryAndWholeRange() {
        for (int sdk = 1; sdk <= 33; sdk++) {
            Sink s = new Sink();
            ApiGate.applyDataSensitive(sdk, s);
            T.eq(Integer.valueOf(0), Integer.valueOf(s.calls), "sdk " + sdk + " must not set it");
        }
        for (int sdk = 34; sdk <= 40; sdk++) {
            Sink s = new Sink();
            ApiGate.applyDataSensitive(sdk, s);
            T.eq(Integer.valueOf(1), Integer.valueOf(s.calls), "sdk " + sdk + " must set it");
        }
        T.ok(!ApiGate.dataSensitiveSupported(29), "API 29 (min SDK) is below the gate");
    }
}
