/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.
 *
 *    Copyright 2018 (c) Mark Giraud, Fraunhofer IOSB
 *    Copyright 2024 (c) Fraunhofer IOSB (Author: Noel Graf)
 */

#ifndef UA_PLUGIN_CERTIFICATEGROUP_H
#define UA_PLUGIN_CERTIFICATEGROUP_H

#include <open62541/types.h>
#include <open62541/plugin/log.h>

_UA_BEGIN_DECLS

struct UA_CertificateGroup;
typedef struct UA_CertificateGroup UA_CertificateGroup;

/**
 * CertificateGroup Plugin API
 * ---------------------------
 *
 * This plugin verifies that the origin of the certificate is trusted. It does
 * not assign any access rights/roles to the holder of the certificate.
 *
 * Usually, implementations of the CertificateGroup plugin provide an
 * initialization method that takes a trust-list and a revocation-list as input.
 * This initialization method takes a pointer to the plugin location and therein
 * calls the ``clear`` method if valid, before attempting to initialize it anew.
 * The lifecycle of the plugin is attached to a server or client config. The
 * ``clear`` method is called automatically when the config is destroyed. */

struct UA_CertificateGroup {
    /* The NodeId of the certificate group this pki store is associated with */
    UA_NodeId certificateGroupId;

    /* Context-pointer to be set by the CertificateGroup plugin implementation */
    void *context;

    /* Pointer to logging pointer in the server/client configuration. If the
     * logging pointer is changed outside of the plugin, the new logger is used
     * automatically. */
    const UA_Logger *logging;

    UA_StatusCode (*getTrustList)(UA_CertificateGroup *certGroup,
                                  UA_TrustListDataType *trustList);

    UA_StatusCode (*setTrustList)(UA_CertificateGroup *certGroup,
                                  const UA_TrustListDataType *trustList);

    UA_StatusCode (*addToTrustList)(UA_CertificateGroup *certGroup,
                                    const UA_TrustListDataType *trustList);

    UA_StatusCode (*removeFromTrustList)(UA_CertificateGroup *certGroup,
                                         const UA_TrustListDataType *trustList);

    UA_StatusCode (*getRejectedList)(UA_CertificateGroup *certGroup,
                                     UA_ByteString **rejectedList,
                                     size_t *rejectedListSize);

    /* Provides all associated CRLs for a CA certificate. */
    UA_StatusCode (*getCertificateCrls)(UA_CertificateGroup *certGroup,
                                        const UA_ByteString *certificate,
                                        const UA_Boolean isTrusted,
                                        UA_ByteString **crls,
                                        size_t *crlsSize);

    UA_StatusCode (*verifyCertificate)(UA_CertificateGroup *certGroup,
                                       const UA_ByteString *certificate);

    void (*clear)(UA_CertificateGroup *certGroup);
};

/* Gives a copy of the first URI of the subject alternative name of the
 * certificate. BadSecurityChecksFailed when the certificate does not parse, and
 * BadCertificateUriInvalid when it has no such name, or the name holds no URI or an
 * empty first one. */
UA_EXPORT UA_StatusCode
UA_CertificateUtils_getApplicationUri(const UA_ByteString *certificate,
                                      UA_String *applicationUri);

/* Why an entry of a trust list does not load. */
typedef enum {
    UA_TRUSTLOAD_GOOD,
    UA_TRUSTLOAD_EMPTY,    /* No certificate in DER or PEM. */
    UA_TRUSTLOAD_INVALID,  /* The certificate, or a damaged PEM block, fails. */
    UA_TRUSTLOAD_TRAILING, /* Bytes after the certificate in its PEM block. */
    UA_TRUSTLOAD_NOMEMORY
} UA_TrustLoad;

/* Loads each certificate of the entry as the memory group loads a trust entry, and
 * gives why it does not load, with the number from 1 of the certificate that fails
 * in `number`. In DER, bytes after the last certificate are one more. */
UA_EXPORT UA_TrustLoad
UA_CertificateUtils_checkTrust(const UA_ByteString *trust, size_t *number);

/* BadCertificateTimeInvalid when `now` is outside the validity window of the
 * certificate, as the verify of the memory group tests a leaf, and
 * BadCertificateInvalid when it does not parse. */
UA_EXPORT UA_StatusCode
UA_CertificateUtils_checkTime(const UA_ByteString *certificate, UA_DateTime now);

/* Verify that the certificate has the applicationURI in the subject name. */
UA_EXPORT UA_StatusCode
UA_CertificateUtils_verifyApplicationUri(const UA_ByteString *certificate,
                                         const UA_String *applicationUri);

/* Get the expire date from certificate */
UA_EXPORT UA_StatusCode
UA_CertificateUtils_getExpirationDate(UA_ByteString *certificate,
                                      UA_DateTime *expiryDateTime);

UA_EXPORT UA_StatusCode
UA_CertificateUtils_getSubjectName(UA_ByteString *certificate,
                                   UA_String *subjectName);

UA_EXPORT UA_StatusCode
UA_CertificateUtils_getThumbprint(UA_ByteString *certificate,
                                  UA_String *thumbprint);

UA_EXPORT UA_StatusCode
UA_CertificateUtils_getKeySize(UA_ByteString *certificate,
                               size_t *keySize);

/* Compares the public keys from two byte strings, which can represent either
 * certificates or Certificate Signing Requests (CSR). This function extracts
 * the public keys from the provided byte strings and compares them to determine
 * if they are identical.
 *
 * @param certificate1 Containing either a certificate or a CSR.
 * @param certificate2 Containing either a certificate or a CSR.
 * @return UA_STATUSCODE_GOOD if the public keys are identical,
 *         UA_STATUSCODE_BADNOMATCH if the public keys do not match,
 *         UA_STATUSCODE_BADINTERNALERROR if an error occurs. */
UA_EXPORT UA_StatusCode
UA_CertificateUtils_comparePublicKeys(const UA_ByteString *certificate1,
                                      const UA_ByteString *certificate2);

UA_EXPORT UA_StatusCode
UA_CertificateUtils_checkKeyPair(const UA_ByteString *certificate,
                                 const UA_ByteString *privateKey);

UA_EXPORT UA_StatusCode
UA_CertificateUtils_checkCA(const UA_ByteString *certificate);

/* Decrypt a private key in PEM format using a password. The output is the key
 * in the binary DER format. Also succeeds if the PEM private key does not
 * require a password or is already in the DER format. The outDerKey memory is
 * allocated internally.
 *
 * Returns UA_STATUSCODE_BADSECURITYCHECKSFAILED if the password is wrong. */
UA_EXPORT UA_StatusCode
UA_CertificateUtils_decryptPrivateKey(const UA_ByteString privateKey,
                                      const UA_ByteString password,
                                      UA_ByteString *outDerKey);

_UA_END_DECLS

#endif /* UA_PLUGIN_CERTIFICATEGROUP_H */
