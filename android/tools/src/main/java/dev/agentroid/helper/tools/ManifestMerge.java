package dev.agentroid.helper.tools;

import java.io.File;
import java.io.StringWriter;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.HashSet;
import java.util.Set;

import javax.xml.XMLConstants;
import javax.xml.parsers.DocumentBuilderFactory;
import javax.xml.transform.OutputKeys;
import javax.xml.transform.Transformer;
import javax.xml.transform.TransformerFactory;
import javax.xml.transform.dom.DOMSource;
import javax.xml.transform.stream.StreamResult;

import org.w3c.dom.Document;
import org.w3c.dom.Element;
import org.w3c.dom.Node;

/**
 * Stand-in for Gradle's manifest merger, JDK only. Takes app/src/main/AndroidManifest.xml as the base, adds the
 * uses-permission entries and the application children (comments included, so the "EXPORTED:" justifications travel
 * with their components) of core, a11y, ime, link, keeper and overlay (one app since 11 Nov 2026), and sets package and version. Used by build-helper.sh and by
 * the checklist audit test, so the audit reads exactly what goes into the APK.
 *
 * Usage: java ManifestMerge ANDROID_DIR OUT_FILE
 */
public final class ManifestMerge {
    private ManifestMerge() { }

    public static final String ANDROID_NS = "http://schemas.android.com/apk/res/android";
    public static final String PACKAGE = "dev.agentroid.helper";
    private static final String[] LIBS = {"core", "a11y", "ime", "link", "keeper", "overlay"};

    public static Document merge(File androidDir) throws Exception {
        Document base = parse(new File(androidDir, "app/src/main/AndroidManifest.xml"));
        Element root = base.getDocumentElement();
        root.setAttribute("package", PACKAGE);
        root.setAttributeNS(ANDROID_NS, "android:versionCode", "1");
        root.setAttributeNS(ANDROID_NS, "android:versionName", "0.1.0-v0");
        Element app = (Element) root.getElementsByTagName("application").item(0);

        Set<String> perms = new HashSet<String>();
        for (Node n = root.getFirstChild(); n != null; n = n.getNextSibling()) {
            if (n instanceof Element && n.getNodeName().equals("uses-permission")) perms.add(((Element) n).getAttributeNS(ANDROID_NS, "name"));
        }

        for (String lib : LIBS) {
            File f = new File(androidDir, lib + "/src/main/AndroidManifest.xml");
            Document d = parse(f);
            for (Node n = d.getDocumentElement().getFirstChild(); n != null; n = n.getNextSibling()) {
                if (n.getNodeType() == Node.COMMENT_NODE) {
                    root.insertBefore(base.importNode(n, true), app);
                } else if (n instanceof Element && n.getNodeName().equals("uses-permission")) {
                    if (perms.add(((Element) n).getAttributeNS(ANDROID_NS, "name"))) root.insertBefore(base.importNode(n, true), app);
                } else if (n instanceof Element && n.getNodeName().equals("application")) {
                    for (Node c = n.getFirstChild(); c != null; c = c.getNextSibling()) {
                        if (c.getNodeType() == Node.TEXT_NODE) continue;
                        app.appendChild(base.importNode(c, true));
                    }
                }
            }
        }
        stripBlankText(root);
        return base;
    }

    private static Document parse(File f) throws Exception {
        DocumentBuilderFactory fac = DocumentBuilderFactory.newInstance();
        fac.setNamespaceAware(true);
        fac.setFeature(XMLConstants.FEATURE_SECURE_PROCESSING, true);
        return fac.newDocumentBuilder().parse(f);
    }

    private static void stripBlankText(Node n) {
        Node c = n.getFirstChild();
        while (c != null) {
            Node next = c.getNextSibling();
            if (c.getNodeType() == Node.TEXT_NODE && c.getNodeValue().trim().isEmpty()) n.removeChild(c);
            else stripBlankText(c);
            c = next;
        }
    }

    public static String toXml(Document d) throws Exception {
        Transformer t = TransformerFactory.newInstance().newTransformer();
        t.setOutputProperty(OutputKeys.INDENT, "yes");
        t.setOutputProperty("{http://xml.apache.org/xslt}indent-amount", "4");
        t.setOutputProperty(OutputKeys.ENCODING, "UTF-8");
        StringWriter w = new StringWriter();
        t.transform(new DOMSource(d), new StreamResult(w));
        return w.toString();
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 2) {
            System.err.println("usage: ManifestMerge ANDROID_DIR OUT_FILE");
            System.exit(2);
        }
        String xml = toXml(merge(new File(args[0])));
        Files.write(new File(args[1]).toPath(), xml.getBytes(StandardCharsets.UTF_8));
    }
}
