package dev.agentroid.helper.link.net;

import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.security.KeyFactory;
import java.security.MessageDigest;
import java.security.PublicKey;
import java.security.SecureRandom;
import java.security.Signature;
import java.security.interfaces.ECPublicKey;
import java.security.spec.ECParameterSpec;
import java.security.spec.X509EncodedKeySpec;
import java.util.Arrays;
import java.util.Base64;

/**
 * Public-key helpers for the link (protocol v1, schemas/link.json). Signing and verifying only: no encryption, no
 * secret keys, no home-made crypto. Base64 here is a transport encoding for public keys, nonces and signatures.
 */
public final class LinkCrypto {
    private LinkCrypto() { }

    public static final String HUB_DOMAIN = "agentroid-link/v1/hub-challenge";
    public static final String PHONE_DOMAIN = "agentroid-link/v1/phone-response";
    public static final int NONCE_MIN_BYTES = 32;
    public static final int NONCE_MAX_BYTES = 64;
    public static final int NONCE_BYTES = 32;

    /** P-256 group order, to recognise the curve without depending on a provider's curve name. */
    private static final String P256_ORDER = "ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551";

    public static String b64(byte[] b) { return Base64.getEncoder().encodeToString(b); }

    /** Strict standard base64: decodes, and the canonical re-encoding must equal the input (no stray bits, no whitespace). */
    public static byte[] unb64(String s) {
        if (s == null) return null;
        try {
            byte[] b = Base64.getDecoder().decode(s);
            return b64(b).equals(s) ? b : null;
        } catch (IllegalArgumentException e) {
            return null;
        }
    }

    /** A fresh nonce of NONCE_BYTES random bytes, base64. */
    public static String newNonce(SecureRandom rnd) {
        byte[] b = new byte[NONCE_BYTES];
        rnd.nextBytes(b);
        return b64(b);
    }

    /** True if s is canonical base64 of 32..64 bytes. */
    public static boolean nonceOk(String s) {
        byte[] b = unb64(s);
        return b != null && b.length >= NONCE_MIN_BYTES && b.length <= NONCE_MAX_BYTES;
    }

    /** Lower-case hex SHA-256 of the SPKI DER. */
    public static String keyId(byte[] spki) {
        try {
            byte[] d = MessageDigest.getInstance("SHA-256").digest(spki);
            StringBuilder s = new StringBuilder(64);
            for (byte x : d) s.append(Character.forDigit((x >> 4) & 0xf, 16)).append(Character.forDigit(x & 0xf, 16));
            return s.toString();
        } catch (GeneralSecurityException e) {
            throw new IllegalStateException("SHA-256 missing", e);
        }
    }

    /** Parses an SPKI DER and insists it is an EC public key on P-256. Throws on anything else (including private keys). */
    public static ECPublicKey p256(byte[] spki) throws GeneralSecurityException {
        if (spki == null || spki.length < 26 || spki.length > 200) throw new GeneralSecurityException("not a P-256 public key");
        PublicKey k = KeyFactory.getInstance("EC").generatePublic(new X509EncodedKeySpec(spki));
        if (!(k instanceof ECPublicKey)) throw new GeneralSecurityException("not an EC public key");
        ECParameterSpec p = ((ECPublicKey) k).getParams();
        if (p == null || !p.getOrder().toString(16).equals(P256_ORDER) || p.getCofactor() != 1) {
            throw new GeneralSecurityException("EC key is not on P-256");
        }
        if (!Arrays.equals(k.getEncoded(), spki)) throw new GeneralSecurityException("SPKI is not canonical");
        return (ECPublicKey) k;
    }

    public static boolean verify(byte[] spki, byte[] msg, byte[] derSig) {
        if (derSig == null || derSig.length < 8 || derSig.length > 72) return false;
        try {
            Signature s = Signature.getInstance(Identity.ALG);
            s.initVerify(p256(spki));
            s.update(msg);
            return s.verify(derSig);
        } catch (GeneralSecurityException | RuntimeException e) {
            return false;
        }
    }

    /** The bytes the HUB signs in a challenge (link.json $defs/challenge). */
    public static byte[] hubChallengeMessage(String nonce, String phoneNonce, long epoch, String serial, String phoneKeyId) {
        return (HUB_DOMAIN + "\nnonce=" + nonce + "\nphone_nonce=" + phoneNonce + "\nepoch=" + epoch
                + "\nserial=" + serial + "\nphone_key_id=" + phoneKeyId).getBytes(StandardCharsets.UTF_8);
    }

    /** The bytes the PHONE signs in a challenge_response (link.json $defs/challenge_response). */
    public static byte[] phoneResponseMessage(String nonce, String phoneNonce, long epoch, String serial, String hubKeyId) {
        return (PHONE_DOMAIN + "\nnonce=" + nonce + "\nphone_nonce=" + phoneNonce + "\nepoch=" + epoch
                + "\nserial=" + serial + "\nhub_key_id=" + hubKeyId).getBytes(StandardCharsets.UTF_8);
    }
}
