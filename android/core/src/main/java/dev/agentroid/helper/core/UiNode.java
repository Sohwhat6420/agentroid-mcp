package dev.agentroid.helper.core;

/** One accessibility node, as far as the serialiser needs. :a11y wraps AccessibilityNodeInfo; tests use a fake. */
public interface UiNode {
    String className();
    String packageName();
    /** Full resource name (pkg:id/name) or null. */
    String viewId();
    String text();
    String contentDescription();
    /** {left, top, right, bottom} in screen pixels. */
    int[] bounds();

    boolean clickable();
    boolean longClickable();
    boolean focusable();
    boolean focused();
    boolean editable();
    boolean enabled();
    boolean visible();
    boolean checkable();
    boolean checked();
    boolean scrollable();
    boolean important();
    /** Password fields never have their text serialised. */
    boolean password();

    /** {min, max, current} for range widgets such as a SeekBar, else null. */
    double[] range();

    int childCount();
    /** May return null (a child that vanished mid-walk). */
    UiNode child(int index);

    /** Called when the serialiser is done with the node (AccessibilityNodeInfo.recycle on API below 33). */
    default void release() { }
}
