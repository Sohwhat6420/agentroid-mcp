package dev.agentroid.helper.core;

import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashSet;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

import javax.xml.parsers.DocumentBuilderFactory;

import org.w3c.dom.Document;
import org.w3c.dom.Element;
import org.w3c.dom.Node;

import dev.agentroid.helper.tools.ManifestMerge;

/**
 * The clean-room checklist, machine-checked where a machine can check it. It reads the MERGED manifest (the same one
 * build-helper.sh packages) and every main source file of the helper modules. Results go to out/checklist.txt, which
 * HELPER-REPORT.md quotes. Properties: helper.root = the android/ folder (default "android").
 */
public class ChecklistAuditTest {
    private static final String NS = ManifestMerge.ANDROID_NS;
    private static final Map<String, String> RESULTS = new LinkedHashMap<String, String>();
    /** 2.12 adds :keeper to the source scan (its manifest is checked by KeeperTest; the helper-flavour merge excludes it). */
    private static final String[] MODULES = {"core", "a11y", "ime", "link", "app", "keeper"};

    private static File root() { return new File(System.getProperty("helper.root", "android")); }

    private static Document manifest() throws Exception { return ManifestMerge.merge(root()); }

    private static Map<String, String> sources() throws Exception {
        Map<String, String> m = new LinkedHashMap<String, String>();
        for (String mod : MODULES) walk(new File(root(), mod + "/src/main/java"), m);
        if (m.isEmpty()) throw new AssertionError("no sources found under " + root().getAbsolutePath());
        return m;
    }

    private static void walk(File d, Map<String, String> out) throws Exception {
        File[] fs = d.listFiles();
        if (fs == null) return;
        for (File f : fs) {
            if (f.isDirectory()) walk(f, out);
            else if (f.getName().endsWith(".java")) {
                String code = stripJavaComments(new String(Files.readAllBytes(f.toPath()), StandardCharsets.UTF_8));
                out.put(f.getPath().replace('\\', '/'), code);
            }
        }
    }

    /**
     * Source with comments blanked (newlines kept), string and char literals left alone, so the audit judges code and a
     * comment that merely mentions a forbidden word cannot fail it (or hide a real use).
     */
    static String stripJavaComments(String s) {
        StringBuilder b = new StringBuilder(s.length());
        int i = 0;
        while (i < s.length()) {
            char c = s.charAt(i);
            if (c == '"' || c == '\'') {
                int j = i + 1;
                while (j < s.length() && s.charAt(j) != c) j += (s.charAt(j) == '\\') ? 2 : 1;
                int end = Math.min(j + 1, s.length());
                b.append(s, i, end);
                i = end;
            } else if (c == '/' && i + 1 < s.length() && s.charAt(i + 1) == '/') {
                while (i < s.length() && s.charAt(i) != '\n') i++;
            } else if (c == '/' && i + 1 < s.length() && s.charAt(i + 1) == '*') {
                int end = s.indexOf("*/", i + 2);
                end = end < 0 ? s.length() : end + 2;
                for (int k = i; k < end; k++) if (s.charAt(k) == '\n') b.append('\n');
                i = end;
            } else {
                b.append(c);
                i++;
            }
        }
        return b.toString();
    }

    public void testItem00_commentStripperIsRight() {
        T.eq("a \"//x\" b \n", stripJavaComments("a \"//x\" b // gone\n"), "line comment after a literal containing //");
        T.eq("x\n\ny", stripJavaComments("x/* a\nb */\ny"), "block comment keeps line count");
        T.eq("c = '\"'; z", stripJavaComments("c = '\"'; /* q */z").replace("  ", " "), "char literal holding a quote");
    }

    private static void item(String name, T.Body b) throws Exception {
        try {
            b.run();
            RESULTS.put(name, "PASS");
        } catch (Exception e) {
            RESULTS.put(name, "FAIL: " + e);
            throw e;
        } catch (Error e) {
            RESULTS.put(name, "FAIL: " + e.getMessage());
            throw e;
        }
    }

    private static List<String> hits(Map<String, String> src, String regex) {
        Pattern p = Pattern.compile(regex);
        List<String> out = new ArrayList<String>();
        for (Map.Entry<String, String> e : src.entrySet()) {
            Matcher m = p.matcher(e.getValue());
            while (m.find()) out.add(e.getKey().substring(e.getKey().lastIndexOf('/') + 1) + ": " + m.group());
        }
        return out;
    }

    private static List<Element> appComponents(Document d) {
        List<Element> out = new ArrayList<Element>();
        Element app = (Element) d.getElementsByTagName("application").item(0);
        for (Node n = app.getFirstChild(); n != null; n = n.getNextSibling()) {
            if (n instanceof Element) out.add((Element) n);
        }
        return out;
    }

    private static String precedingComment(Node n) {
        for (Node p = n.getPreviousSibling(); p != null; p = p.getPreviousSibling()) {
            if (p.getNodeType() == Node.COMMENT_NODE) return p.getNodeValue().trim();
            if (p.getNodeType() == Node.TEXT_NODE && p.getNodeValue().trim().isEmpty()) continue;
            return null;
        }
        return null;
    }

    private static boolean hasIntentFilter(Element e) { return e.getElementsByTagName("intent-filter").getLength() > 0; }

    // ------------------------------------------------------------------ the items

    public void testItem01_allowBackupFalse() throws Exception {
        item("allowBackup=false", () -> {
            Element app = (Element) manifest().getElementsByTagName("application").item(0);
            T.eq("false", app.getAttributeNS(NS, "allowBackup"), "android:allowBackup on <application>");
        });
    }

    public void testItem02_noExportedProviderAndNoProviderAtAll() throws Exception {
        item("no provider (so none exported)", () -> {
            T.eq(Integer.valueOf(0), Integer.valueOf(manifest().getElementsByTagName("provider").getLength()), "<provider> elements");
            T.ok(hits(sources(), "extends\\s+(ContentProvider|FileProvider)").isEmpty(), "no ContentProvider subclass");
        });
    }

    public void testItem03_noWorldReadableFilesOrPrefs() throws Exception {
        item("no world-readable files or prefs", () -> {
            List<String> bad = hits(sources(),
                    "MODE_WORLD_\\w+|setReadable\\(|setWritable\\(|getExternal\\w*|/sdcard|/data/local/tmp|MODE_APPEND|openFileOutput|FileOutputStream|FileWriter");
            T.ok(bad.isEmpty(), "found: " + bad);
            for (Map.Entry<String, String> e : sources().entrySet()) {
                for (String line : e.getValue().split("\n")) {
                    if (line.contains("getSharedPreferences(") && !line.contains("MODE_PRIVATE")) {
                        throw new AssertionError(e.getKey() + ": getSharedPreferences without MODE_PRIVATE: " + line.trim());
                    }
                }
            }
        });
    }

    public void testItem04_noSecretOrConfigReadableOverAdb() throws Exception {
        item("no secret/config readable over adb", () -> {
            Element root = manifest().getDocumentElement();
            T.ok(!((Element) root.getElementsByTagName("application").item(0)).hasAttributeNS(NS, "debuggable"),
                    "manifest source must not set android:debuggable (the debug build flag comes from aapt2 --debug-mode only)");
            // Stored values: the hub address, the laptop's display name and an on/off flag; since 2.12 also the hub's PUBLIC
            // key (Pairing refuses private keys), the serial the hub read, the highest epoch, and the keeper's trusted
            // BSSIDs. Nothing named like a secret; the phone's private key exists only inside AndroidKeyStore.
            Set<String> allowed = new HashSet<String>(Arrays.asList("hub", "laptop", "enabled", "hub_key", "serial", "max_epoch", "trusted_bssids"));
            Pattern put = Pattern.compile("\\.put(?:String|Boolean|Int|Long|Float|StringSet)\\(\"([^\"]+)\"");
            Pattern secretish = Pattern.compile("(?i)token|secret|password|passwd|nonce|signature|private|credential|apikey|api_key");
            int stored = 0;
            for (Map.Entry<String, String> e : sources().entrySet()) {
                Matcher m = put.matcher(e.getValue());
                while (m.find()) {
                    stored++;
                    T.ok(allowed.contains(m.group(1)), e.getKey() + " stores unexpected key '" + m.group(1) + "'");
                    T.ok(!secretish.matcher(m.group(1)).find(), "secret-looking key " + m.group(1));
                }
            }
            T.eq(Integer.valueOf(11), Integer.valueOf(stored),
                    "number of stored values (save: hub laptop enabled; setEnabled; savePairing: hub laptop hub_key serial enabled; max_epoch; trusted_bssids)");
            // Logcat is readable over adb: every Log call must be a fixed string, so no address, name, typed text or screen content.
            Pattern anyLog = Pattern.compile("\\bLog\\.[a-z]\\(");
            Pattern fixedLog = Pattern.compile("\\bLog\\.[viwe]\\(TAG, \"[^\"+]*\"\\);");
            for (Map.Entry<String, String> e : sources().entrySet()) {
                for (String line : e.getValue().split("\n")) {
                    if (anyLog.matcher(line).find() && !fixedLog.matcher(line).find()) {
                        throw new AssertionError(e.getKey() + ": Log call that is not a fixed string: " + line.trim());
                    }
                }
            }
            // 2.12: exactly one dump() override, LinkService's, and it prints DumpText only (public key, key id, alg:
            // IdentityTest checks those four lines). ADR 0001: the hub reads the public key over adb this way.
            List<String> dumps = hits(sources(), "void\\s+dump\\(");
            T.eq(Integer.valueOf(1), Integer.valueOf(dumps.size()), "dump() overrides: " + dumps);
            T.ok(dumps.get(0).startsWith("LinkService.java"), "the one dump() is LinkService's");
            String ls = sources().get(findKey("LinkService.java"));
            String body = ls.substring(ls.indexOf("void dump("), ls.indexOf("}", ls.indexOf("pw.flush()")));
            T.ok(body.contains("pw.print(DumpText.of(") && body.split("pw\\.print").length == 2, "dump() prints DumpText and nothing else");
            T.ok(hits(sources(), "System\\.(out|err)\\.print").isEmpty(), "no System.out/err output");
        });
    }

    public void testItem05_noBase64EncryptionOrHomemadeCrypto() throws Exception {
        item("no base64 'encryption'", () -> {
            // Base64 is a transport encoding in exactly two places: the screenshot PNG (Payload) and, since 2.12, the
            // link's public keys, nonces and signatures (LinkCrypto). Neither "protects" anything.
            for (Map.Entry<String, String> e : sources().entrySet()) {
                boolean allowed = e.getKey().endsWith("/Payload.java") || e.getKey().endsWith("/link/net/LinkCrypto.java");
                if (!allowed) T.ok(!e.getValue().contains("Base64"), e.getKey() + " mentions Base64; only Payload.java and LinkCrypto.java may");
            }
            // No encryption, secret keys or XOR anywhere. 2.12 brings public-key SIGNING only: SHA256withECDSA in
            // LinkCrypto (verify) and KeystoreIdentity (sign), and SHA-256 for the key id in LinkCrypto.
            T.ok(hits(sources(), "javax\\.crypto|\\bCipher\\b|SecretKeySpec|SecretKey\\b|KeyAgreement|\\^\\s*0x").isEmpty(), "no encryption or XOR code");
            for (String h : hits(sources(), "MessageDigest")) T.ok(h.startsWith("LinkCrypto.java"), "MessageDigest outside LinkCrypto: " + h);
            for (String h : hits(sources(), "Signature\\.getInstance\\([^)]*\\)")) {
                T.ok((h.startsWith("LinkCrypto.java") || h.startsWith("KeystoreIdentity.java")) && h.contains("ALG"), "signature use: " + h);
            }
            String ks = sources().get(findKey("KeystoreIdentity.java"));
            T.ok(ks.contains("\"AndroidKeyStore\"") && ks.contains("KeyProperties.PURPOSE_SIGN") && !ks.contains("PURPOSE_ENCRYPT"),
                    "the phone key is a sign-only AndroidKeyStore key");
            // Payload.b64 is used for exactly one thing: the screenshot PNG.
            List<String> uses = hits(sources(), "Payload\\.b64\\(");
            T.eq(Integer.valueOf(1), Integer.valueOf(uses.size()), "Payload.b64 call sites: " + uses);
            T.ok(uses.get(0).startsWith("HelperMethods.java"), "the one call is the screenshot method");
            T.ok(sources().get(findKey("HelperMethods.java")).contains("Payload.b64(s.png)"), "applied to the screenshot bytes only");
        });
    }

    private static String findKey(String suffix) throws Exception {
        for (String k : sources().keySet()) if (k.endsWith("/" + suffix)) return k;
        throw new AssertionError("no source named " + suffix);
    }

    public void testItem06_everyExportedComponentIsJustified() throws Exception {
        item("every exported component justified in a comment", () -> {
            List<Element> comps = appComponents(manifest());
            int exported = 0;
            for (Element c : comps) {
                String name = c.getAttributeNS(NS, "name");
                String ex = c.getAttributeNS(NS, "exported");
                if (ex.isEmpty()) {
                    T.ok(!hasIntentFilter(c), name + " has an intent-filter but no explicit android:exported");
                    throw new AssertionError(name + " has no explicit android:exported");
                }
                if ("true".equals(ex)) {
                    exported++;
                    String why = precedingComment(c);
                    T.ok(why != null && why.startsWith("EXPORTED:") && why.length() > 40, name + " is exported without an 'EXPORTED:' justification comment");
                } else {
                    T.eq("false", ex, name + " exported value");
                }
            }
            // one app since 11 Nov 2026: the keeper's trust receiver joins (DUMP-gated, see item 07)
            T.eq(Integer.valueOf(5), Integer.valueOf(exported), "exported components (launcher activity, a11y service, IME, config receiver, keeper trust receiver)");
        });
    }

    public void testItem07_exportedServicesAndReceiversAreGatedByAPermission() throws Exception {
        item("exported services/receivers need a permission", () -> {
            Map<String, String> perms = new LinkedHashMap<String, String>();
            for (Element c : appComponents(manifest())) {
                if (!"true".equals(c.getAttributeNS(NS, "exported"))) continue;
                String tag = c.getTagName();
                if (tag.equals("activity")) continue; // the launcher activity: no input, see its comment
                String p = c.getAttributeNS(NS, "permission");
                T.ok(!p.isEmpty(), c.getAttributeNS(NS, "name") + " is exported with no android:permission");
                perms.put(c.getAttributeNS(NS, "name"), p);
            }
            T.eq("android.permission.DUMP", perms.get("dev.agentroid.helper.link.ConfigReceiver"), "config receiver permission");
            T.eq("android.permission.DUMP", perms.get("dev.agentroid.helper.keeper.KeeperConfigReceiver"), "keeper trust receiver permission");
            T.eq("android.permission.BIND_ACCESSIBILITY_SERVICE", perms.get("dev.agentroid.helper.a11y.HelperAccessibilityService"), "a11y permission");
            T.eq("android.permission.BIND_INPUT_METHOD", perms.get("dev.agentroid.helper.ime.HelperInputMethodService"), "IME permission");
        });
    }

    public void testItem08_noListeningSocket() throws Exception {
        item("no listening socket (dial-out only, ADR 0001)", () -> {
            T.ok(hits(sources(), "ServerSocket|ServerSocketChannel|HttpServer|NanoHTTPD|\\.listen\\(|DatagramSocket").isEmpty(), "server-side socket classes");
        });
    }

    public void testItem09_cleartextIsScopedToTheTailnet() throws Exception {
        item("cleartext ws:// only to tailnet hosts (2.12: 100.64.0.0/10 only)", () -> {
            Element app = (Element) manifest().getElementsByTagName("application").item(0);
            T.eq("true", app.getAttributeNS(NS, "usesCleartextTraffic"), "usesCleartextTraffic (needed for ws://)");
            T.ok(sources().get(findKey("HubSpec.java")).contains("allowedHost"), "HubSpec gates the host");
            // the only place a URL is built is HubSpec.url()
            T.ok(hits(sources(), "\"wss?://[^\"]*\"").size() == 1, "one hard-coded URL scheme literal: " + hits(sources(), "\"wss?://[^\"]*\""));
            // 2.12: LinkClient refuses anything outside the tailnet before dialling and builds the URL with HubSpec;
            // OkHttp resolves through the tailnet-only Dns with no proxy; LinkService starts the client from the store;
            // both config actions apply the same guard. AddressTest checks the behaviour.
            String lc = sources().get(findKey("LinkClient.java"));
            T.ok(lc.contains("TailnetGuard.checkHost(host);") && lc.contains("HubSpec.parse(host + \":\" + port, null).url()"), "LinkClient guards and uses HubSpec.url()");
            String ok = sources().get(findKey("OkHttpTransport.java"));
            T.ok(ok.contains(".dns(new TailnetDns(Dns.SYSTEM))") && ok.contains(".proxy(java.net.Proxy.NO_PROXY)"), "OkHttp: tailnet Dns, no proxy");
            T.ok(sources().get(findKey("LinkService.java")).contains("client.start(hub.host, hub.port)"), "LinkService dials the stored hub through LinkClient");
            String cr = sources().get(findKey("ConfigReceiver.java"));
            T.ok(cr.contains("TailnetGuard.checkHost(spec.host)") && cr.contains("Pairing.parse("), "CONFIGURE and PAIR both refuse non-tailnet hosts");
            T.ok(hits(sources(), "newWebSocket\\(").size() == 1 && hits(sources(), "newWebSocket\\(").get(0).startsWith("OkHttpTransport.java"),
                    "one WebSocket dial site: " + hits(sources(), "newWebSocket\\("));
        });
    }

    public void testItem10_accessibilityConfig() throws Exception {
        item("a11y config flags and capabilities", () -> {
            File f = new File(root(), "a11y/src/main/res/xml/helper_access.xml");
            DocumentBuilderFactory fac = DocumentBuilderFactory.newInstance();
            fac.setNamespaceAware(true);
            Element e = fac.newDocumentBuilder().parse(f).getDocumentElement();
            Set<String> flags = new HashSet<String>(Arrays.asList(e.getAttributeNS(NS, "accessibilityFlags").split("\\|")));
            T.eq(new HashSet<String>(Arrays.asList("flagIncludeNotImportantViews", "flagReportViewIds", "flagRetrieveInteractiveWindows")), flags, "flags");
            T.eq("true", e.getAttributeNS(NS, "canTakeScreenshot"), "canTakeScreenshot");
            T.eq("true", e.getAttributeNS(NS, "canPerformGestures"), "canPerformGestures");
            T.eq("true", e.getAttributeNS(NS, "canRetrieveWindowContent"), "canRetrieveWindowContent");
        });
    }

    public void testItem11_foregroundNotificationIsVisibleAndStoppable() throws Exception {
        item("foreground notification: ongoing, names laptop, Stop action, re-posts", () -> {
            String s = sources().get(findKey("LinkService.java"));
            for (String must : new String[] {"setOngoing(true)", "addAction(", "\"Stop\"", "setDeleteIntent(", "areNotificationsEnabled()",
                    "FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE", "laptopLabel", "NotificationText.body("}) {
                T.ok(s.contains(must), "LinkService lacks " + must);
            }
            for (Element c : appComponents(manifest())) {
                if (c.getAttributeNS(NS, "name").endsWith(".LinkService")) {
                    T.eq("false", c.getAttributeNS(NS, "exported"), "LinkService exported");
                    T.eq("connectedDevice", c.getAttributeNS(NS, "foregroundServiceType"), "foregroundServiceType");
                }
            }
        });
    }

    public void testItem12_dataSensitiveIsVersionGated() throws Exception {
        item("accessibilityDataSensitive only via the API-34 gate", () -> {
            int users = 0;
            for (Map.Entry<String, String> e : sources().entrySet()) {
                if (e.getValue().contains("setAccessibilityDataSensitive(")) {
                    users++;
                    T.ok(e.getValue().contains("ApiGate.applyDataSensitive(Build.VERSION.SDK_INT"), e.getKey() + " sets the flag outside the gate");
                }
            }
            T.eq(Integer.valueOf(2), Integer.valueOf(users), "files that set it (DataSensitive, MainActivity)");
        });
    }

    // One app since 11 Nov 2026: the keeper's WRITE_SECURE_SETTINGS is declared, but Android grants it only through
    // `pm grant` over adb, so on a phone with no adb (the no-ADB route) it is never held. Still no storage permission.
    public void testItem13_permissionListIsExactlyTheExpectedEight() throws Exception {
        item("permission list exact (WRITE_SECURE_SETTINGS only from the keeper, no storage)", () -> {
            Set<String> got = new HashSet<String>();
            Element root = manifest().getDocumentElement();
            for (Node n = root.getFirstChild(); n != null; n = n.getNextSibling()) {
                if (n instanceof Element && n.getNodeName().equals("uses-permission")) got.add(((Element) n).getAttributeNS(NS, "name"));
            }
            Set<String> want = new HashSet<String>();
            for (String p : new String[] {"POST_NOTIFICATIONS", "INTERNET", "ACCESS_NETWORK_STATE", "FOREGROUND_SERVICE",
                    "FOREGROUND_SERVICE_CONNECTED_DEVICE", "CHANGE_WIFI_MULTICAST_STATE", "RECEIVE_BOOT_COMPLETED", "WRITE_SECURE_SETTINGS"}) want.add("android.permission." + p);
            T.eq(want, got, "uses-permission set");
        });
    }

    public void testItem14_buildScriptTargetsSdk29To36AndNeverTouchesAPhone() throws Exception {
        item("build script: min 29 / target 36, no adb", () -> {
            String s = new String(Files.readAllBytes(new File(root(), "build-helper.sh").toPath()), StandardCharsets.UTF_8);
            T.ok(s.contains("--min-sdk-version 29") && s.contains("--target-sdk-version 36"), "SDK levels");
            T.ok(!Pattern.compile("(^|[\\s;&|(])adb(\\.exe)?\\s", Pattern.MULTILINE).matcher(stripComments(s)).find(), "build script runs no adb");
            T.ok(!stripComments(s).contains("start ") && !stripComments(s).contains("cmd /c"), "no window-popping commands");
        });
    }

    private static String stripComments(String sh) {
        StringBuilder b = new StringBuilder();
        for (String line : sh.split("\n")) {
            if (line.trim().startsWith("#")) continue;
            b.append(line).append('\n');
        }
        return b.toString();
    }

    /** Last in name order: writes the table the report quotes. Items that failed already failed their own test. */
    public void testItem99_writeChecklist() throws Exception {
        List<String> lines = new ArrayList<String>();
        for (Map.Entry<String, String> e : RESULTS.entrySet()) lines.add(e.getValue().startsWith("PASS") ? "PASS  " + e.getKey() : "FAIL  " + e.getKey() + "  (" + e.getValue() + ")");
        lines.add("ATTESTED (manual, not machine-checkable)  clean room: no mobilerun-portal code was read, opened or copied into this tree");
        File out = new File(root(), "out/checklist.txt");
        out.getParentFile().mkdirs();
        Files.write(out.toPath(), (String.join("\n", lines) + "\n").getBytes(StandardCharsets.UTF_8));
        for (String l : lines) System.out.println("  checklist: " + l);
        T.eq(Integer.valueOf(14 - 0), Integer.valueOf(RESULTS.size()), "all 14 machine-checked items ran before the writer");
        for (String v : RESULTS.values()) T.ok(v.startsWith("PASS"), "a checklist item failed: " + v);
    }
}
