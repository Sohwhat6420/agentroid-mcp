package dev.agentroid.helper.link.net;

import java.security.GeneralSecurityException;
import java.security.KeyFactory;
import java.security.spec.PKCS8EncodedKeySpec;

import dev.agentroid.helper.core.HubSpec;

/**
 * Checks the pairing broadcast (link.ConfigReceiver, action dev.agentroid.helper.PAIR, protected by
 * android.permission.DUMP so only the adb shell can send it):
 * <pre>
 *   adb shell am broadcast -n dev.agentroid.helper/.link.ConfigReceiver -a dev.agentroid.helper.PAIR \
 *       --es hub_key &lt;base64 SPKI DER of the hub's P-256 PUBLIC key&gt; --es hub 100.101.102.103:8765 \
 *       --es serial &lt;ro.serialno&gt; [--es laptop "Alex-PC"] [--ez rekey true]
 * </pre>
 * Only a public key is accepted. Private key material is refused outright and never stored. The address must be in
 * 100.64.0.0/10 or a *.ts.net name.
 */
public final class Pairing {
    public final byte[] hubKeySpki;
    public final HubSpec hub;
    public final String serial;

    private Pairing(byte[] hubKeySpki, HubSpec hub, String serial) {
        this.hubKeySpki = hubKeySpki;
        this.hub = hub;
        this.serial = serial;
    }

    public static final class Invalid extends Exception {
        public Invalid(String m) { super(m); }
    }

    public static Pairing parse(String hubKeyB64, String hubAddr, String serial, String laptop) throws Invalid {
        if (hubKeyB64 == null || hubKeyB64.isEmpty()) throw new Invalid("hub_key is missing");
        byte[] der = LinkCrypto.unb64(hubKeyB64.trim());
        if (der == null) throw new Invalid("hub_key is not canonical base64");
        if (looksPrivate(der)) throw new Invalid("hub_key is a PRIVATE key; send only the hub's public key (SPKI)");
        try {
            LinkCrypto.p256(der);
        } catch (GeneralSecurityException | RuntimeException e) {
            throw new Invalid("hub_key is not an EC P-256 public key (X.509 SPKI DER)");
        }
        HubSpec spec;
        try {
            spec = HubSpec.parse(hubAddr, laptop);
            TailnetGuard.checkHost(spec.host);
        } catch (HubSpec.Invalid | TailnetGuard.Refused e) {
            throw new Invalid("hub address refused: " + e.getMessage());
        }
        if (serial == null || !LinkProto.SERIAL.matcher(serial).matches()) throw new Invalid("serial must be 1..64 of [A-Za-z0-9._:-]");
        return new Pairing(der, spec, serial);
    }

    private static boolean looksPrivate(byte[] der) {
        try {
            KeyFactory.getInstance("EC").generatePrivate(new PKCS8EncodedKeySpec(der));
            return true;
        } catch (GeneralSecurityException | RuntimeException e) {
            return false;
        }
    }
}
