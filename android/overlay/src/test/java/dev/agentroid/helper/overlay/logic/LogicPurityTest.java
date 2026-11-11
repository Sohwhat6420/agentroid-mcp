package dev.agentroid.helper.overlay.logic;

import static dev.agentroid.helper.overlay.logic.OverlayTestRunner.ok;

import java.io.File;
import java.nio.charset.Charset;
import java.nio.file.Files;

/** The logic package stays plain Java (no android.* import), so every rule above is tested on the JVM. */
public class LogicPurityTest {
    public void testLogicImportsNoAndroidClass() throws Exception {
        String root = System.getProperty("overlay.root");
        ok(root != null, "run through run-overlay-tests.sh (sets -Doverlay.root)");
        File dir = new File(root, "src/main/java/dev/agentroid/helper/overlay/logic");
        File[] files = dir.listFiles();
        ok(files != null && files.length >= 5, "logic sources found in " + dir);
        for (File f : files) {
            String src = new String(Files.readAllBytes(f.toPath()), Charset.forName("UTF-8"));
            ok(!src.contains("import android."), f.getName() + " imports android.*");
        }
    }
}
