package dev.agentroid.bankguard;

import android.content.ContentProvider;
import android.content.ContentValues;
import android.database.Cursor;
import android.database.MatrixCursor;
import android.net.Uri;

/**
 * content://dev.agentroid.bankguard.hold answers one row, one column: held (1 or 0). The Agentroid keeper asks it
 * before it switches Wireless debugging back on, so the two apps never fight over the setting.
 *
 * <p>Readable by any app on purpose, and read-only. It tells nothing a reader cannot already see: any app can read
 * Settings.Global adb_enabled and adb_wifi_enabled, and so see that debugging was switched off. It never says which
 * app is open. A hold the guard stopped refreshing (a dead guard) reads as 0 after {@link GuardLogic#HOLD_STALE_MS}.
 */
public final class HoldProvider extends ContentProvider {
    @Override public boolean onCreate() { return true; }

    @Override
    public Cursor query(Uri uri, String[] projection, String selection, String[] args, String sort) {
        MatrixCursor c = new MatrixCursor(new String[] {"held"}, 1);
        c.addRow(new Object[] {GuardCore.get(getContext()).holdLive() ? 1 : 0});
        return c;
    }

    @Override public String getType(Uri uri) { return null; }
    @Override public Uri insert(Uri uri, ContentValues v) { throw new UnsupportedOperationException("read-only"); }
    @Override public int delete(Uri uri, String s, String[] a) { throw new UnsupportedOperationException("read-only"); }
    @Override public int update(Uri uri, ContentValues v, String s, String[] a) { throw new UnsupportedOperationException("read-only"); }
}
