package dev.agentroid.helper.runner;

import java.util.Map;

/**
 * Where the runner writes what it decided and did: every refusal (whole segment or per step), every step's start
 * (before acting) and outcome, and the queued report. Records are flat JSON objects
 * {v, ts_ms, mono_ms, segment_id, step_id?, status, decision?, reason?}. FileJournal is the durable one.
 */
public interface RunnerJournal {
    void append(Map<String, Object> record);
}
