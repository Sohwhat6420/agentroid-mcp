package dev.agentroid.helper.core;

/** One accessibility window (with not-important views included, so the system shade and dialogs show up). */
public interface UiWindow {
    int id();
    /** APPLICATION, INPUT_METHOD, SYSTEM, ACCESSIBILITY_OVERLAY, SPLIT_SCREEN_DIVIDER, ACTIVE_FALLBACK, OTHER_n. */
    String type();
    int layer();
    boolean active();
    boolean focused();
    /** {left, top, right, bottom} or null when unknown. */
    int[] bounds();
    UiNode root();
}
