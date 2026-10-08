//! The OpenSSH form of an Ed25519 public key.

use std::fmt;

use ssh_key::HashAlg;
use ssh_key::public::Ed25519PublicKey;
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
/// base64 of the key, and a comment, which is optional and not kept. As OpenSSH reads
/// it, the comment is the rest of the line, so it may hold another key.
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
    // `ssh-key` splits the words only at one space, and OpenSSH at any run of spaces
    // and tabs. `from_openssh` accepts a key length field over 32 when 32 bytes
    // follow, so the key must write back to the same line.
    let line = format!("{ALGORITHM} {encoded}");
    let key = ssh_key::PublicKey::from_openssh(&line).or(Err(Error::NotEd25519))?;
    if key.to_openssh().ok().as_deref() != Some(line.as_str()) {
        return Err(Error::NotEd25519);
    }
    let Some(&Ed25519PublicKey(bytes)) = key.key_data().ed25519() else {
        unreachable!(
            "invariant: `ssh-key` refuses a key of another algorithm than the text"
        );
    };
    PublicKey::new(bytes).map_err(Error::SmallOrder)
}

/// The `SHA256:` fingerprint of `key`, as `ssh-keygen -l` writes it.
#[must_use]
pub fn fingerprint(key: PublicKey) -> String {
    ssh_key::PublicKey::from(Ed25519PublicKey(key.to_bytes()))
        .fingerprint(HashAlg::Sha256)
        .to_string()
}
