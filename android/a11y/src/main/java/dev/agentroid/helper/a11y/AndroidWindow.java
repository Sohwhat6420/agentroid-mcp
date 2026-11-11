package dev.agentroid.helper.a11y;

import dev.agentroid.helper.core.UiNode;
import dev.agentroid.helper.core.UiWindow;

/** A window's facts, captured when the walk starts, plus its root node. */
final class AndroidWindow implements UiWindow {
    private final int id;
    private final String type;
    private final int layer;
    private final boolean active;
    private final boolean focused;
    private final int[] bounds;
    private final AndroidNode root;

    AndroidWindow(int id, String type, int layer, boolean active, boolean focused, int[] bounds, AndroidNode root) {
        this.id = id;
        this.type = type;
        this.layer = layer;
        this.active = active;
        this.focused = focused;
        this.bounds = bounds;
        this.root = root;
    }

    @Override public int id() { return id; }
    @Override public String type() { return type; }
    @Override public int layer() { return layer; }
    @Override public boolean active() { return active; }
    @Override public boolean focused() { return focused; }
    @Override public int[] bounds() { return bounds; }
    @Override public UiNode root() { return root; }
}
