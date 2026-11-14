package dev.agentroid.bankguard;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;

/**
 * content://dev.agentroid.bankguard.hold answers one row, one column: held (1 or 0), meaning "keeper, leave Wireless
 * debugging off now". The Agentroid keeper asks it before it switches Wireless debugging back on, so the two apps
 * never fight over the setting.
 *
 * <p>Readable by any app on purpose, and read-only. It answers 1 only when the guard itself switched Wireless
 * debugging off (a live hold that will put it back, or a release that had to leave it off), so it tells a reader no
 * more than Settings.Global adb_wifi_enabled already does: that Wireless debugging is off. It never says which app is
 * open, and a bank opened with Wireless debugging already off reads 0. A hold the guard stopped refreshing (a dead
 * guard) reads 0 after {@link GuardLogic#HOLD_STALE_MS}.
 */
public final class HoldProvider extends ContentProvider {
    @Override public boolean onCreate() { return true; }

    @Override
    public Cursor query(Uri uri, String[] projection, String selection, String[] args, String sort) {
        MatrixCursor c = new MatrixCursor(new String[] {"held"}, 1);
        c.addRow(new Object[] {GuardCore.get(getContext()).keeperShouldWait() ? 1 : 0});
        return c;
    }

    @Override public String getType(Uri uri) { return null; }
    @Override public Uri insert(Uri uri, ContentValues v) { throw new UnsupportedOperationException("read-only"); }
    @Override public int delete(Uri uri, String s, String[] a) { throw new UnsupportedOperationException("read-only"); }
    @Override public int update(Uri uri, ContentValues v, String s, String[] a) { throw new UnsupportedOperationException("read-only"); }
}
