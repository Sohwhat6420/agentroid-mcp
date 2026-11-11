package dev.agentroid.helper.runner;

import dev.agentroid.helper.core.Json;

import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.charset.StandardCharsets;
import java.util.Map;

/**
 * Append-only JSON-lines journal; each record is written and fsynced before append returns, so a "started" record
 * is on disk before the step acts (a crash leaves an intent the resume path can check, as 2.10b's journal does).
 * In the app the file lives in the device-protected data dir (works before first unlock), never /data/local/tmp.
 */
public final class FileJournal implements RunnerJournal {
    private final FileOutputStream out;

    public FileJournal(File f) throws IOException {
        this.out = new FileOutputStream(f, true);
    }

    @Override
    public synchronized void append(Map<String, Object> record) {
        try {
            out.write((Json.write(record) + "\n").getBytes(StandardCharsets.UTF_8));
            out.getFD().sync();
        } catch (IOException e) {
            throw new IllegalStateException("journal write failed", e);
        }
    }

    public synchronized void close() throws IOException { out.close(); }
}
