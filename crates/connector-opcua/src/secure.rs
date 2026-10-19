//! The test peers of secure channels.
//!
//! The fixtures in `secure/` are self-signed RSA 2048 certificates with SHA-256 and
//! their PKCS #1 keys, in DER from Python `cryptography` 41.0.7, each valid from
//! [`START`] to [`END`] other than [`LASTING`], PEM copies of them, and one P-256
//! certificate and its key. They are test keys only.

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
