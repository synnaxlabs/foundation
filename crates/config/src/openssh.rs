use std::fmt;

use base64ct::{Base64, Encoding};
use types::ed25519::PublicKey;

const ALGORITHM: &str = "ssh-ed25519";
/// The name of each other algorithm of an OpenSSH public key. A message names only
/// these, since another first word can be a secret.
const OTHER_ALGORITHMS: [&str; 17] = [
    "ssh-rsa",
    "ssh-dss",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ecdsa-sha2-nistp256@openssh.com",
    "sk-ssh-ed25519@openssh.com",
    "ssh-rsa-cert-v01@openssh.com",
    "ssh-dss-cert-v01@openssh.com",
    "ecdsa-sha2-nistp256-cert-v01@openssh.com",
    "ecdsa-sha2-nistp384-cert-v01@openssh.com",
    "ecdsa-sha2-nistp521-cert-v01@openssh.com",
    "sk-ecdsa-sha2-nistp256-cert-v01@openssh.com",
    "ssh-ed25519-cert-v01@openssh.com",
    "sk-ssh-ed25519-cert-v01@openssh.com",
    "ssh-xmss@openssh.com",
    "ssh-xmss-cert-v01@openssh.com",
];
/// Each Unicode line break.
const LINE_BREAKS: [char; 7] =
    ['\n', '\x0b', '\x0c', '\r', '\u{85}', '\u{2028}', '\u{2029}'];
/// The decoded key of an Ed25519 line starts with the length and the name of its
/// algorithm, then the length of the key.
const BLOB_START: &[u8; 19] = b"\0\0\0\x0bssh-ed25519\0\0\0\x20";
/// The length of the decoded key of an Ed25519 line: [`BLOB_START`] and the key.
const BLOB_BYTES: usize = 51;

/// Why a text is not the line of an OpenSSH `.pub` file of an Ed25519 key. Its message
/// quotes no part of the text after the first word, since that part can be a secret.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// It has no algorithm and key, or its first word is no known algorithm.
    NotALine,
    /// The key is of another known algorithm.
    Algorithm(&'static str),
    /// It is more than one line.
    Lines,
    /// The base64 does not decode to an Ed25519 key.
    NotEd25519,
    /// The key is a point of small order.
    SmallOrder(types::ed25519::SmallOrder),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotALine => {
                f.write_str("the public key is not the line of a `.pub` file")
            }
            Self::Algorithm(algorithm) => {
                let only = "and a subject takes only";
                write!(f, "the public key is {algorithm:?}, {only} `{ALGORITHM}`")
            }
            Self::Lines => f.write_str("the public key is more than one line"),
            Self::NotEd25519 => {
                f.write_str("the base64 of the public key is not an Ed25519 key")
            }
            Self::SmallOrder(error) => error.fmt(f),
        }
    }
}

/// Reads the line of an OpenSSH `.pub` file of an Ed25519 key: `ssh-ed25519`, the
/// base64 of the key, and a comment, which is optional and not kept.
pub(crate) fn public_key(text: &str) -> Result<PublicKey, Error> {
    let mut words = text.split_ascii_whitespace();
    let (Some(algorithm), Some(encoded)) = (words.next(), words.next()) else {
        return Err(Error::NotALine);
    };
    if algorithm != ALGORITHM {
        let known = OTHER_ALGORITHMS.iter().find(|other| **other == algorithm);
        return Err(known.map_or(Error::NotALine, |other| Error::Algorithm(other)));
    }
    if text.trim().contains(LINE_BREAKS) {
        return Err(Error::Lines);
    }
    let mut blob = [0; BLOB_BYTES];
    let bytes = Base64::decode(encoded, &mut blob)
        .ok()
        .and_then(|blob| blob.strip_prefix(BLOB_START))
        .and_then(|key| <[u8; 32]>::try_from(key).ok())
        .ok_or(Error::NotEd25519)?;
    PublicKey::new(bytes).map_err(Error::SmallOrder)
}
