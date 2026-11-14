package dev.agentroid.bankguard;

import android.content.Context;
import android.graphics.Bitmap;
import android.graphics.Canvas;
import android.graphics.Paint;
import android.graphics.Typeface;
import android.util.TypedValue;

/**
 * Draws a line of DotoClock text into a bitmap for the clock widgets.
 *
 * Why a bitmap: a launcher inflates a widget's RemoteViews in a restricted context, and TextView loads a resource
 * font only when the context is not restricted. So `android:fontFamily="@font/..."` falls back to the system font in
 * every widget (seen on the POCO, 14 Nov 2026). The font loads fine here, in this app's own process.
 *
 * A line box is 1.2 em tall with the baseline at 0.95 em, the same as the font's own ascent and descent. The layouts'
 * margins were measured against that box, so they still hold.
 */
final class ClockRender {
    private static Typeface face;

    private ClockRender() { }

    static synchronized Typeface face(Context c) {
        if (face == null) {
            try { face = c.getResources().getFont(R.font.doto_clock); }
            catch (Throwable t) { face = Typeface.MONOSPACE; }
        }
        return face;
    }

    /**
     * Segments drawn left to right on one baseline, segment i in colour i, gapEm apart. With shadow, a soft dark
     * shadow keeps white type legible on a light wallpaper (the clock has no surface of its own).
     *
     * strokeEm above 0 grows every dot by that much (an outline of that width around each one). At 0.02 em the dots
     * of a line nearly touch, which is how the hello clock's time looks in the original widget (measured side by side,
     * 14 Nov 2026: its dots are about 3 px wider at the same dot pitch).
     */
    static Bitmap line(Context c, float sp, String[] text, int[] colour, float gapEm, boolean shadow, float strokeEm) {
        float px = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_SP, sp, c.getResources().getDisplayMetrics());
        float dp = c.getResources().getDisplayMetrics().density;
        Paint p = new Paint(Paint.ANTI_ALIAS_FLAG);
        p.setTypeface(face(c));
        p.setTextSize(px);
        float pad = 0;
        if (strokeEm > 0) {
            p.setStyle(Paint.Style.FILL_AND_STROKE);
            p.setStrokeWidth(strokeEm * px);
            pad = strokeEm * px / 2;
        }
        if (shadow) {
            pad += 3 * dp;
            p.setShadowLayer(3 * dp, 0, 0, 0x59000000);
        }
        float gap = gapEm * px, w = 0;
        float[] widths = new float[text.length];
        for (int i = 0; i < text.length; i++) {
            widths[i] = p.measureText(text[i]);
            w += widths[i] + (i > 0 ? gap : 0);
        }
        w -= 0.1f * px; // every glyph's advance ends in one empty cell (0.1 em): drop the last one
        int bw = Math.max(1, (int) Math.ceil(w + 2 * pad));
        int bh = Math.max(1, (int) Math.ceil(1.2f * px));
        Bitmap b = Bitmap.createBitmap(bw, bh, Bitmap.Config.ARGB_8888);
        Canvas cv = new Canvas(b);
        float x = pad, base = 0.95f * px;
        for (int i = 0; i < text.length; i++) {
            p.setColor(colour[i]);
            cv.drawText(text[i], x, base, p);
            x += widths[i] + gap;
        }
        return b;
    }

    static Bitmap line(Context c, float sp, String text, int colour, boolean shadow) {
        return line(c, sp, new String[] {text}, new int[] {colour}, 0, shadow, 0);
    }

    static Bitmap line(Context c, float sp, String[] text, int[] colour, float gapEm, boolean shadow) {
        return line(c, sp, text, colour, gapEm, shadow, 0);
    }
}
