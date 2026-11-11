/*
 * The tree walk behind this adapter follows Artemis HierarchyDumper.java (Apache-2.0; packages/artemis-accessibility-helper,
 * com.artemis.helper), lines 463-513 (multi-window enumeration sorted by layer, hash de-duplication, active-window
 * fallback), as already carried in the S4 spike. The "not important" filter at its line 104 is deliberately NOT carried over.
 * No mobilerun-portal code was read or copied (AGPL; clean-room, see HELPER-REPORT.md).
 *
 * Apache-2.0 section 4(b) notice: this file is derived from google/artemis (Copyright Google LLC, Apache License 2.0;
 * upstream has no NOTICE file) and has been modified by the Agentroid authors (adapted to the UiNode interface, with
 * the "not important" filter removed). The licence text is in the LICENSE file at the repository root.
 */
package dev.agentroid.helper.a11y;

import android.graphics.Rect;
import android.os.Build;
import android.view.accessibility.AccessibilityNodeInfo;

import dev.agentroid.helper.core.UiNode;

/** AccessibilityNodeInfo seen through the core's UiNode. release() recycles on API below 33, where recycle still matters. */
final class AndroidNode implements UiNode {
    final AccessibilityNodeInfo n;

    AndroidNode(AccessibilityNodeInfo n) { this.n = n; }

    private static String s(CharSequence c) { return c == null ? null : c.toString(); }

    @Override public String className() { return s(n.getClassName()); }
    @Override public String packageName() { return s(n.getPackageName()); }
    @Override public String viewId() { return n.getViewIdResourceName(); }
    @Override public String text() { return s(n.getText()); }
    @Override public String contentDescription() { return s(n.getContentDescription()); }

    @Override public int[] bounds() {
        Rect r = new Rect();
        n.getBoundsInScreen(r);
        return new int[] {r.left, r.top, r.right, r.bottom};
    }

    @Override public boolean clickable() { return n.isClickable(); }
    @Override public boolean longClickable() { return n.isLongClickable(); }
    @Override public boolean focusable() { return n.isFocusable(); }
    @Override public boolean focused() { return n.isFocused(); }
    @Override public boolean editable() { return n.isEditable(); }
    @Override public boolean enabled() { return n.isEnabled(); }
    @Override public boolean visible() { return n.isVisibleToUser(); }
    @Override public boolean checkable() { return n.isCheckable(); }
    @Override public boolean checked() { return n.isChecked(); }
    @Override public boolean scrollable() { return n.isScrollable(); }
    @Override public boolean important() { return n.isImportantForAccessibility(); }
    @Override public boolean password() { return n.isPassword(); }

    @Override public double[] range() {
        AccessibilityNodeInfo.RangeInfo ri = n.getRangeInfo();
        if (ri == null) return null;
        return new double[] {ri.getMin(), ri.getMax(), ri.getCurrent()};
    }

    @Override public int childCount() { return n.getChildCount(); }

    @Override public UiNode child(int index) {
        AccessibilityNodeInfo c = n.getChild(index);
        return c == null ? null : new AndroidNode(c);
    }

    @Override public void release() { recycle(n); }

    @SuppressWarnings("deprecation")
    static void recycle(AccessibilityNodeInfo info) {
        if (Build.VERSION.SDK_INT < 33) info.recycle();
    }
}
