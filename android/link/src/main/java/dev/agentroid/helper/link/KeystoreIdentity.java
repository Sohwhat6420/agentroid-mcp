package dev.agentroid.helper.link;

import android.security.keystore.KeyGenParameterSpec;
import android.security.keystore.KeyProperties;
import android.security.keystore.StrongBoxUnavailableException;

import java.security.GeneralSecurityException;
import java.security.Key;
import java.security.KeyPairGenerator;
import java.security.KeyStore;
import java.security.PrivateKey;
import java.security.Signature;
import java.security.cert.Certificate;
import java.security.spec.ECGenParameterSpec;

import dev.agentroid.helper.link.net.Identity;

/**
 * The phone's link key (task 2.12): an EC P-256 key generated INSIDE AndroidKeyStore, sign-only,
 * SHA-256 digest. AndroidKeyStore keys are non-exportable by construction: the private key object is a handle, its
 * key material never enters this process (getEncoded() on it returns null), so it cannot be serialised, logged, backed
 * up or pushed anywhere, and nothing is ever written to /data/local/tmp. StrongBox is used when the phone has one,
 * otherwise the TEE. Only the public key leaves, through LinkService.dump() and the hello's key id.
 *
 * Direct boot: the key is created without user authentication or an unlocked-device requirement, so it is meant to be
 * usable before the first unlock; that is unverified on a phone (the POCO has no lock, so it cannot show it).
 */
final class KeystoreIdentity implements Identity {
    static final String ALIAS = "agentroid-link-v1";
    private static final String STORE = "AndroidKeyStore";

    private KeyStore open() throws GeneralSecurityException {
        try {
            KeyStore ks = KeyStore.getInstance(STORE);
            ks.load(null);
            return ks;
        } catch (java.io.IOException e) {
            throw new GeneralSecurityException("keystore unavailable");
        }
    }

    private KeyStore ready() throws GeneralSecurityException {
        KeyStore ks = open();
        if (!ks.containsAlias(ALIAS)) generate();
        return ks;
    }

    private void generate() throws GeneralSecurityException {
        try {
            generate(true);
        } catch (StrongBoxUnavailableException e) {
            generate(false);
        }
    }

    private void generate(boolean strongBox) throws GeneralSecurityException {
        KeyGenParameterSpec.Builder b = new KeyGenParameterSpec.Builder(ALIAS, KeyProperties.PURPOSE_SIGN)
                .setAlgorithmParameterSpec(new ECGenParameterSpec("secp256r1"))
                .setDigests(KeyProperties.DIGEST_SHA256)
                .setUserAuthenticationRequired(false);
        if (strongBox) b.setIsStrongBoxBacked(true);
        KeyPairGenerator g = KeyPairGenerator.getInstance(KeyProperties.KEY_ALGORITHM_EC, STORE);
        g.initialize(b.build());
        g.generateKeyPair();
    }

    @Override
    public synchronized byte[] publicKeySpki() throws GeneralSecurityException {
        Certificate cert = ready().getCertificate(ALIAS);
        if (cert == null) throw new GeneralSecurityException("no certificate for the link key");
        return cert.getPublicKey().getEncoded();
    }

    @Override
    public synchronized byte[] sign(byte[] msg) throws GeneralSecurityException {
        Key k = ready().getKey(ALIAS, null);
        if (!(k instanceof PrivateKey)) throw new GeneralSecurityException("link key missing");
        Signature s = Signature.getInstance(ALG);
        s.initSign((PrivateKey) k);
        s.update(msg);
        return s.sign();
    }

    /** `agentroid link pair <serial> --rekey`: drop the old key and make a new one (the hub then re-reads the dump). */
    synchronized void rekey() throws GeneralSecurityException {
        KeyStore ks = open();
        if (ks.containsAlias(ALIAS)) ks.deleteEntry(ALIAS);
        generate();
    }
}
