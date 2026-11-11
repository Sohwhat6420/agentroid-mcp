package dev.agentroid.helper.link.net;

import java.security.GeneralSecurityException;
import java.security.KeyPair;
import java.security.KeyPairGenerator;
import java.security.PrivateKey;
import java.security.SecureRandom;
import java.security.Signature;
import java.security.spec.ECGenParameterSpec;

/**
 * TEST ONLY: a software P-256 key behind {@link Identity}, standing in for the AndroidKeyStore key. Like the real one it
 * exposes the public key and signatures only. {@link #privateEncodedForLeakCheck} exists so the tests can search
 * every output for the private key's bytes; nothing in the product can call it (this class is not in src/main).
 */
final class SoftwareIdentity implements Identity {
    private final PrivateKey priv;
    private final byte[] spki;
    private boolean broken;

    SoftwareIdentity(long seed) {
        try {
            KeyPairGenerator g = KeyPairGenerator.getInstance("EC");
            SecureRandom r = SecureRandom.getInstance("SHA1PRNG");
            r.setSeed(seed);
            g.initialize(new ECGenParameterSpec("secp256r1"), r);
            KeyPair kp = g.generateKeyPair();
            priv = kp.getPrivate();
            spki = kp.getPublic().getEncoded();
        } catch (GeneralSecurityException e) {
            throw new IllegalStateException(e);
        }
    }

    /** Make sign() fail, as a Keystore that refuses the key would. */
    void breakKey() { broken = true; }

    @Override public byte[] publicKeySpki() { return spki.clone(); }

    @Override
    public byte[] sign(byte[] msg) throws GeneralSecurityException {
        if (broken) throw new GeneralSecurityException("key refused");
        Signature s = Signature.getInstance(ALG);
        s.initSign(priv);
        s.update(msg);
        return s.sign();
    }

    byte[] privateEncodedForLeakCheck() { return priv.getEncoded(); }

    java.math.BigInteger privateScalarForLeakCheck() { return ((java.security.interfaces.ECPrivateKey) priv).getS(); }
}
