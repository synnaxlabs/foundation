//! The test peers of secure channels.
//!
//! The fixtures in `secure/` are self-signed RSA 2048 certificates with SHA-256 and
//! their PKCS #1 keys, in DER from Python `cryptography` 41.0.7, each valid from
//! [`START`] to [`END`] other than [`LASTING`], PEM copies of them, and one P-256
//! certificate and its key. They are test keys only. `nameless.der` is from `openssl
//! req -x509`: a CA certificate with an empty subject and issuer and no subject
//! alternative name, valid from 2026-10-20 to 2027-10-20.

use std::ffi::CStr;

use types::time::{Span, Stamp};

/// 2001-01-01.
pub(crate) const START: Stamp = from_secs(978_307_200);

/// 2002-01-01.
pub(crate) const END: Stamp = from_secs(1_009_843_200);

/// The test server.
pub(crate) const SERVER: Identity = Identity {
    certificate: include_bytes!("secure/server.der"),
    key: include_bytes!("secure/server-key.der"),
};

/// The test client.
pub(crate) const CLIENT: Identity = Identity {
    certificate: include_bytes!("secure/client.der"),
    key: include_bytes!("secure/client-key.der"),
};

/// A client on the key of [`CLIENT`] whose subject alternative name holds no URI.
pub(crate) const ANONYMOUS: Identity = Identity {
    certificate: include_bytes!("secure/anonymous.der"),
    key: CLIENT.key,
};

/// A client on the key of [`CLIENT`] with no subject alternative name.
pub(crate) const UNNAMED: Identity = Identity {
    certificate: include_bytes!("secure/unnamed.der"),
    key: CLIENT.key,
};

/// A certificate on the key of [`CLIENT`] with an empty subject.
pub(crate) const NAMELESS: &[u8] = include_bytes!("secure/nameless.der");

/// [`CLIENT`] as the certificate of a CA.
pub(crate) const CA_CLIENT: &[u8] = include_bytes!("secure/ca-client.der");

/// A certificate on the key of [`CLIENT`] whose subject is `/CN=client` and 18 units
/// `/OU=<nn>` with 58 `x`, past 1024 bytes in the one-line form.
pub(crate) const LONG_SUBJECT: &[u8] = include_bytes!("secure/long-subject.der");

/// A certificate on the key of [`CLIENT`] whose subject is a description of 1100 `d`,
/// then `/CN=client`.
pub(crate) const LONG_ENTRY: &[u8] = include_bytes!("secure/long-entry.der");

/// A client on the key of [`CLIENT`] whose first URI is empty.
pub(crate) const EMPTY_URI: Identity = Identity {
    certificate: include_bytes!("secure/empty-uri.der"),
    key: CLIENT.key,
};

/// A client on the key of [`CLIENT`] with the URI of [`CLIENT`], then another.
pub(crate) const TWO_URIS: Identity = Identity {
    certificate: include_bytes!("secure/two-uris.der"),
    key: CLIENT.key,
};

/// [`CLIENT`] valid from 2000-01-01 to 2003-01-01, a year before [`START`] and after
/// [`END`].
pub(crate) const LASTING: Identity = Identity {
    certificate: include_bytes!("secure/lasting.der"),
    key: CLIENT.key,
};

/// A server with the URI of [`SERVER`] and another key.
pub(crate) const OTHER: Identity = Identity {
    certificate: include_bytes!("secure/other.der"),
    key: include_bytes!("secure/other-key.der"),
};

/// [`CLIENT`] in PEM.
pub(crate) const CLIENT_PEM: Identity = Identity {
    certificate: include_bytes!("secure/client.pem"),
    key: include_bytes!("secure/client-key.pem"),
};

/// [`CLIENT_PEM`] with a byte of 0xA0 before the `\n` of the second line of its
/// certificate and of its key.
pub(crate) const HIGH_PEM: Identity = Identity {
    certificate: &HIGH_CERTIFICATE,
    key: &HIGH_KEY,
};

const HIGH_CERTIFICATE: [u8; CLIENT_PEM.certificate.len() + 1] =
    with_high_byte(CLIENT_PEM.certificate);

const HIGH_KEY: [u8; CLIENT_PEM.key.len() + 1] = with_high_byte(CLIENT_PEM.key);

/// `pem` with a byte of 0xA0 before the `\n` of its second line.
///
/// # Panics
///
/// When `pem` has fewer than two `\n`, or `LENGTH` is not one past its length.
const fn with_high_byte<const LENGTH: usize>(pem: &[u8]) -> [u8; LENGTH] {
    let mut out = [0; LENGTH];
    let (mut from, mut to, mut lines) = (0, 0, 0);
    while from < pem.len() {
        if pem[from] == b'\n' {
            lines += 1;
            if lines == 2 {
                out[to] = 0xA0;
                to += 1;
            }
        }
        out[to] = pem[from];
        (from, to) = (from + 1, to + 1);
    }
    assert!(
        to == LENGTH,
        "the PEM has two lines, and LENGTH is one past its length"
    );
    out
}

/// [`CLIENT`] with its key in a PKCS #8 whose one attribute holds [`PAD`] bytes, so the
/// key is past the 100 KiB that a DER read through a BIO takes.
pub(crate) const LONG_KEY: Identity = Identity {
    certificate: CLIENT.certificate,
    key: &LONG_KEY_DER,
};

const PAD: usize = 100 << 10;

static LONG_KEY_DER: [u8; CLIENT.key.len() + PAD + 58] = long_key(CLIENT.key);

/// The PKCS #8 of the RSA PKCS #1 key `key`, with one attribute, `unstructuredName`
/// of PKCS #9, whose value is an OCTET STRING of [`PAD`] zero bytes.
///
/// # Panics
///
/// When `LENGTH` is not the length of the PKCS #8, or `key` is not 256 to 65 535
/// bytes long.
const fn long_key<const LENGTH: usize>(key: &[u8]) -> [u8; LENGTH] {
    // The version 0, then the algorithm rsaEncryption with NULL parameters.
    const HEAD: [u8; 18] = [
        0x02, 0x01, 0x00, 0x30, 0x0D, 0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D,
        0x01, 0x01, 0x01, 0x05, 0x00,
    ];
    // The OID 1.2.840.113549.1.9.2.
    const NAME: [u8; 11] = [
        0x06, 0x09, 0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x02,
    ];
    assert!(key.len() >= 256 && key.len() <= 0xFFFF, "a two-byte length");
    let mut out = [0; LENGTH];
    let mut at = header(&mut out, 0, 0x30, LENGTH - 5);
    at = copy(&mut out, at, &HEAD);
    at = header(&mut out, at, 0x04, key.len());
    at = copy(&mut out, at, key);
    at = header(&mut out, at, 0xA0, PAD + 26);
    at = header(&mut out, at, 0x30, PAD + 21);
    at = copy(&mut out, at, &NAME);
    at = header(&mut out, at, 0x31, PAD + 5);
    at = header(&mut out, at, 0x04, PAD);
    assert!(at + PAD == LENGTH, "the length of the PKCS #8");
    out
}

/// Writes at `at` the DER header of `tag` with the `length` of 256 to 2^24 - 1 bytes,
/// and gives the index after it.
const fn header(out: &mut [u8], at: usize, tag: u8, length: usize) -> usize {
    let (count, form) = if length >> 16 == 0 {
        (2, 0x82)
    } else {
        (3, 0x83)
    };
    let be = length.to_be_bytes();
    out[at] = tag;
    out[at + 1] = form;
    copy(out, at + 2, be.split_at(be.len() - count).1)
}

/// Copies `bytes` to `out` at `at`, and gives the index after them.
const fn copy(out: &mut [u8], at: usize, bytes: &[u8]) -> usize {
    let mut index = 0;
    while index < bytes.len() {
        out[at + index] = bytes[index];
        index += 1;
    }
    at + bytes.len()
}

/// A client with the URI of [`CLIENT`] on a P-256 key.
pub(crate) const EC: Identity = Identity {
    certificate: include_bytes!("secure/ec.der"),
    key: include_bytes!("secure/ec-key.der"),
};

/// [`OTHER`], then [`SERVER`], in PEM.
pub(crate) const TRUST: &[u8] = include_bytes!("secure/trust.pem");

/// [`TRUST`] after the 81 bytes of text that `openssl pkcs12 -nokeys` writes before a
/// certificate.
pub(crate) const HEADED_TRUST: &[u8] = concat!(
    "Bag Attributes\n",
    "    localKeyID: 01 00 00 00 \n",
    "subject=CN = other\n",
    "issuer=CN = other\n",
    include_str!("secure/trust.pem"),
)
.as_bytes();

/// A certificate whose common name is the first line of a PEM certificate, then
/// [`SERVER`], in DER.
pub(crate) const LINE_TRUST: &[u8] = include_bytes!("secure/line-trust.der");

/// [`SERVER`], then the first 300 bytes of [`OTHER`], in PEM.
pub(crate) const BROKEN_TRUST: &[u8] = include_bytes!("secure/broken-trust.pem");

/// [`SERVER`] as a `TRUSTED CERTIFICATE` that rejects server authentication, as
/// `openssl x509 -addreject serverAuth -trustout` writes it.
pub(crate) const REJECTED_SERVER: &[u8] = include_bytes!("secure/server-rejected.pem");

/// [`SERVER`] as a `TRUSTED CERTIFICATE` that trusts it only for client
/// authentication, as `openssl x509 -addtrust clientAuth -trustout` writes it.
pub(crate) const CLIENT_ONLY_SERVER: &[u8] =
    include_bytes!("secure/server-for-clients.pem");

/// The DER of [`SERVER`] and one zero byte, in the base64 of one PEM block.
pub(crate) const LONG_BLOCK_TRUST: &[u8] =
    include_bytes!("secure/long-block-trust.pem");

/// The DER of [`OTHER`], then of [`SERVER`], in the base64 of one PEM block.
pub(crate) const ONE_BLOCK_TRUST: &[u8] = include_bytes!("secure/one-block-trust.pem");

const fn from_secs(secs: i64) -> Stamp {
    Stamp::from_nanos(secs * Span::SECOND.nanos())
}

/// A certificate and its key.
#[derive(Clone, Copy)]
pub(crate) struct Identity {
    pub(crate) certificate: &'static [u8],
    pub(crate) key: &'static [u8],
}

/// The three policies that the copy compiles.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Policy {
    Basic256Sha256,
    Aes128Sha256RsaOaep,
    Aes256Sha256RsaPss,
}

impl Policy {
    pub(crate) const ALL: [Self; 3] = [
        Self::Basic256Sha256,
        Self::Aes128Sha256RsaOaep,
        Self::Aes256Sha256RsaPss,
    ];

    /// The URI of the policy.
    pub(crate) fn uri(self) -> &'static CStr {
        match self {
            Self::Basic256Sha256 => {
                c"http://opcfoundation.org/UA/SecurityPolicy#Basic256Sha256"
            }
            Self::Aes128Sha256RsaOaep => {
                c"http://opcfoundation.org/UA/SecurityPolicy#Aes128_Sha256_RsaOaep"
            }
            Self::Aes256Sha256RsaPss => {
                c"http://opcfoundation.org/UA/SecurityPolicy#Aes256_Sha256_RsaPss"
            }
        }
    }
}
