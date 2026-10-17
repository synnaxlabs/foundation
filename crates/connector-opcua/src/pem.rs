//! Checks that the PEM readers of the open62541 copy use no password that the caller
//! does not give, and never prompt on the console.
//!
//! The fixtures in `pem/` come from OpenSSL 3.0.13 and Python `cryptography` 41.0.7.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::ffi::c_void;

use crate::ffi::{self, Bytes, Status};

unsafe extern "C" {
    fn UA_CertificateUtils_checkKeyPair(
        certificate: *const Bytes,
        key: *const Bytes,
    ) -> u32;
    fn UA_CertificateUtils_checkCA(certificate: *const Bytes) -> u32;
    fn UA_CertificateUtils_decryptPrivateKey(
        key: Bytes,
        password: Bytes,
        out: *mut Bytes,
    ) -> u32;
    #[cfg(feature = "sim")]
    fn shim_verify_certificate(certificate: *const Bytes) -> u32;
}

/// A self-signed P-256 CA certificate (`CN=breaker`).
const CERTIFICATE: &str = include_str!("pem/certificate.pem");

/// The key of [`CERTIFICATE`] in a traditional PEM, encrypted with the password
/// `secret` (`openssl ec -aes256`).
const ENCRYPTED_KEY: &str = include_str!("pem/encrypted-key.pem");

/// [`CERTIFICATE`] in a PEM with `Proc-Type: 4,ENCRYPTED` (AES-256-CBC and the password
/// `secret`, as `PEM_ASN1_write_bio` writes it), which `openssl x509 -passin` reads.
const ENCRYPTED_CERTIFICATE: &str = include_str!("pem/encrypted-certificate.pem");

/// A P-256 key in PKCS #8, encrypted with the empty password (`openssl pkcs8 -topk8
/// -v2 aes-128-cbc -passout pass:`).
const EMPTY_KEY: &str = include_str!("pem/empty-key.pem");

/// A self-signed certificate of [`EMPTY_KEY`] (`openssl req -x509`).
const EMPTY_CERTIFICATE: &str = include_str!("pem/empty-certificate.pem");

/// A P-256 key in PKCS #8, encrypted with the password `x`.
const KEY: &str = include_str!("pem/key.pem");

/// A P-256 key in PKCS #8 (PBES2, PBKDF2 with HMAC-SHA256, AES-128-CBC), encrypted with
/// 1024 bytes `a`, the buffer of aws-lc. A short Python script encoded it, because
/// `cryptography` refuses a password of more than 1023 bytes.
const LONG_KEY: &str = include_str!("pem/long-key.pem");

/// The variable that names the read that `read` makes.
const READ: &str = "CONNECTOR_OPCUA_PEM";

/// The password of each encrypted fixture, as a person types it at a prompt.
#[cfg(unix)]
const TYPED: &[u8] = b"secret\n";

fn bytes(text: &str) -> Bytes {
    Bytes {
        length: text.len(),
        data: text.as_ptr().cast_mut(),
    }
}

/// Makes the read that `READ` names, and asserts its status. Does nothing when `READ`
/// is not set.
#[test]
fn read() {
    #[expect(
        clippy::disallowed_methods,
        reason = "the parent test picks the read that its child process makes"
    )]
    let Ok(name) = std::env::var(READ) else {
        return;
    };
    let pair = |certificate, key| {
        // SAFETY: both point at live bytes, which the call only reads.
        Status(unsafe {
            UA_CertificateUtils_checkKeyPair(&bytes(certificate), &bytes(key))
        })
    };
    let status = match name.as_str() {
        "key" => pair(CERTIFICATE, ENCRYPTED_KEY),
        "empty key" => pair(EMPTY_CERTIFICATE, EMPTY_KEY),
        // SAFETY: it points at live bytes, which the call only reads.
        "certificate" => Status(unsafe {
            UA_CertificateUtils_checkCA(&bytes(ENCRYPTED_CERTIFICATE))
        }),
        #[cfg(feature = "sim")]
        "remote certificate" => {
            // SAFETY: as above.
            Status(unsafe { shim_verify_certificate(&bytes(ENCRYPTED_CERTIFICATE)) })
        }
        _ => panic!("no read is named {name}"),
    };
    let expected = if name == "remote certificate" {
        "BadCertificateInvalid"
    } else {
        "BadSecurityChecksFailed"
    };
    assert_eq!(status.name(), expected, "{name}");
}

/// The reads that `read` makes.
#[cfg(unix)]
const READS: &[&str] = &[
    "key",
    "empty key",
    "certificate",
    #[cfg(feature = "sim")]
    "remote certificate",
];

/// A node has no terminal and no password to give. Each read refuses an encrypted PEM
/// with no prompt, whether its standard input holds the password or is closed. The
/// policies load their key through `checkKeyPair`, and the remote certificate of a
/// channel through `UA_OpenSSL_LoadCertificate`, as `checkCA` does, and through the
/// certificate group.
#[test]
#[cfg(unix)]
#[cfg_attr(not(target_os = "linux"), ignore = "needs setsid")]
fn each_read_refuses_an_encrypted_pem_with_no_prompt() {
    for name in READS {
        for stdin in [TYPED, b""] {
            let output = crate::child::detached("pem::read", &[(READ, name)], stdin);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success()
                    && stdout.contains("test result: ok. 1 passed")
                    && stderr.is_empty(),
                "{name} with {stdin:?}: {stdout}{stderr}"
            );
        }
    }
}

/// The status of `UA_CertificateUtils_decryptPrivateKey` on `key` with `password`.
fn decrypt(key: &str, password: &[u8]) -> Status {
    let mut key = key.as_bytes().to_vec();
    let mut password = password.to_vec();
    let key = Bytes {
        length: key.len(),
        data: key.as_mut_ptr(),
    };
    let password = Bytes {
        length: password.len(),
        data: password.as_mut_ptr(),
    };
    let mut out = Bytes {
        length: 0,
        data: std::ptr::null_mut(),
    };
    // SAFETY: each `Bytes` points at its live bytes, and `out` is empty.
    let status = Status(unsafe {
        UA_CertificateUtils_decryptPrivateKey(key, password, &raw mut out)
    });
    // SAFETY: `out` is an empty `UA_ByteString`, or one that the call allocated.
    unsafe {
        ffi::UA_clear(
            (&raw mut out).cast::<c_void>(),
            &raw const ffi::types()[ffi::BYTE_STRING],
        );
    }
    status
}

/// In a child process: decrypts with passwords at and past the buffer of aws-lc.
#[test]
fn decrypt_each_password() {
    if !crate::child::running() {
        return;
    }
    assert_eq!(decrypt(KEY, b"x").name(), "Good");
    assert_eq!(decrypt(KEY, b"y").name(), "BadSecurityChecksFailed");
    assert_eq!(decrypt(LONG_KEY, &[b'a'; 1024]).name(), "Good");
    for length in [1025, 4096] {
        let status = decrypt(LONG_KEY, &vec![b'a'; length]);
        assert_eq!(status.name(), "BadSecurityChecksFailed", "{length}");
    }
    let status = decrypt(EMPTY_KEY, &[b'a'; 1025]);
    assert_eq!(status.name(), "BadSecurityChecksFailed");
}

/// A password that does not fit in the buffer of aws-lc gives an error, and no read or
/// write past the buffer, which would end the child process.
#[test]
fn a_password_longer_than_the_buffer_of_aws_lc_is_refused() {
    crate::child::run("pem::decrypt_each_password", &[]);
}

/// `UA_Openssl_Init` keeps no state, as it has work only with the API of OpenSSL before
/// 1.1, so a build that asks for that API fails, and the object of its file has no
/// zeroed static.
#[test]
fn the_copy_refuses_a_build_that_defines_openssl_api_compat() {
    let copy = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../patches/open62541");
    let flags = std::fs::read_to_string(copy.join("flags.txt")).unwrap();
    let object = std::path::Path::new(env!("OUT_DIR")).join("api-compat.o");
    let check = |define: bool| {
        let mut library = crate::compiler::builds(&copy, &flags, "").library;
        library.include(env!("CONNECTOR_OPCUA_LIBCRYPTO"));
        if define {
            library.define("OPENSSL_API_COMPAT", "0x10000000L");
        }
        let compiler = crate::child::tool(&mut library, env!("CONNECTOR_OPCUA_TARGET"));
        let file = copy.join("plugins/crypto/openssl/securitypolicy_common.c");
        let output = compiler
            .to_command()
            .arg("-c")
            .arg("-o")
            .arg(&object)
            .arg(file)
            .output()
            .unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    let (built, stderr) = check(false);
    assert!(built, "{stderr}");
    let output = std::process::Command::new("nm")
        .arg("-P")
        .arg(&object)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let symbols = String::from_utf8(output.stdout).unwrap();
    let zeroed: Vec<_> = symbols
        .lines()
        .filter(|line| matches!(line.split_whitespace().nth(1), Some("b" | "B")))
        .collect();
    assert!(zeroed.is_empty(), "{zeroed:?}");
    let (built, stderr) = check(true);
    assert!(
        !built && stderr.contains("\"the copy builds no OpenSSL API before 1.1\""),
        "{stderr}"
    );
}
