package dev.agentroid.helper.link.net;

/**
 * What LinkService.dump() prints for `adb shell dumpsys activity service dev.agentroid.helper/.link.LinkService`
 * (the caller needs android.permission.DUMP, which the adb shell holds). ONLY public material: the phone's public key,
 * its key id and the algorithm, plus the protocol version. No address, serial, epoch, nonce or hub detail. One
 * "key=value" per line so the hub can parse it without guessing:
 * <pre>
 *   agentroid-link v1
 *   alg=SHA256withECDSA
 *   public_key=&lt;base64 X.509 SPKI DER&gt;
 *   key_id=&lt;hex SHA-256 of the SPKI&gt;
 * </pre>
 * When the key cannot be read, the last two lines are replaced by "error=key_unavailable".
 */
public final class DumpText {
    private DumpText() { }

    public static String of(Identity id) {
        StringBuilder b = new StringBuilder("agentroid-link v1\nalg=").append(Identity.ALG).append('\n');
        try {
            byte[] spki = id.publicKeySpki();
            b.append("public_key=").append(LinkCrypto.b64(spki)).append('\n');
            b.append("key_id=").append(LinkCrypto.keyId(spki)).append('\n');
        } catch (Exception e) {
            b.append("error=key_unavailable\n");
        }
        return b.toString();
    }
}
