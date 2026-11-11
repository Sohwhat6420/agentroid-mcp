package dev.agentroid.helper.runner;

import java.util.Map;

/**
 * How the runner touches the phone. The real implementation is accessibility glue (not wired in 2.14; see
 * RUNNER-PHONE-REPORT.md): count/read over AccessibilityNodeInfo from getWindows(), act through
 * ACTION_CLICK / ACTION_SCROLL_FORWARD|BACKWARD / performGlobalAction(GLOBAL_ACTION_BACK), settings through
 * Settings.System|Secure|Global.putString under the 2.13 lease. Calls come on the runner's Clock thread and must be
 * short: anything slow (an animation, a download) is waited for by the post-condition poll, not inside act().
 */
public interface StepExecutor {
    /** Nodes on screen (display 0, interactive windows) matching a one-key selector {id|text|desc|near_text: value}. */
    int count(Map<String, Object> select);

    /** The text of the single node matching select, or null if there is not exactly one. */
    String read(Map<String, Object> select);

    /**
     * Do a tap, scroll, back or settings_set step. Returns null when the action was performed, else why not
     * (the step then fails and the segment stops).
     */
    String act(Segment.Step step);

    /** The current value of a setting, or null when absent (for the setting_equals post-condition). */
    String setting(String namespace, String key);
}
