package dev.agentroid.helper.link.net;

import java.security.GeneralSecurityException;

/**
 * The phone's link identity: an EC P-256 key pair whose private half never leaves its store. In the app it is a
 * NON-EXPORTABLE AndroidKeyStore key (link.KeystoreIdentity); the JVM tests use a software key behind this same
 * interface. Deliberately, nothing here returns the private key, and implementations must not be Serializable: only
 * the public key (X.509 SubjectPublicKeyInfo DER) and signatures ever come out.
 */
public interface Identity {
    String ALG = "SHA256withECDSA";

    /** X.509 SubjectPublicKeyInfo DER of the public key. Safe to show anywhere. */
    byte[] publicKeySpki() throws GeneralSecurityException;

    /** SHA256withECDSA over msg, ASN.1 DER encoded. */
    byte[] sign(byte[] msg) throws GeneralSecurityException;
}
