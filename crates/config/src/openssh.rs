//! The OpenSSH forms of an Ed25519 key: the line of a `.pub` file, and the private key
//! file.

use std::fmt;

use document::diagnostic::{Code, Diagnostic};
use document::value::{Kind, Value};
use ssh_key::private::{Ed25519Keypair, Ed25519PrivateKey, KeypairData};
use ssh_key::public::Ed25519PublicKey;
use ssh_key::{HashAlg, LineEnding};
use types::ed25519::{PrivateKey, PublicKey};

const BAD_PUBLIC_KEY: Code = Code::new("config.bad-public-key");
const PUBLIC_KEY_ALGORITHM: Code = Code::new("config.public-key-algorithm");
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

/// What holds a public key: its noun in a message, and the fixes of its caller.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Holder {
    /// The holder in a message, such as "a subject".
    pub noun: &'static str,
    /// The fix of a key that is not the line of a `.pub` file of an Ed25519 key.
    pub fix: &'static str,
    /// The fix of a key of another algorithm.
    pub algorithm_fix: &'static str,
}

/// Reads the public key of `holder`: the line of an OpenSSH `.pub` file of an Ed25519
/// key. A message quotes no part of the text after the first word, since that part can
/// be a secret.
pub(crate) fn key(value: &Value, holder: Holder) -> Result<PublicKey, Diagnostic> {
    let Holder {
        noun,
        fix,
        algorithm_fix,
    } = holder;
    let Kind::String(text) = &value.kind else {
        let message = format!("a public key is a string, not {}", value.kind.noun());
        return Err(Diagnostic::new(
            BAD_PUBLIC_KEY,
            value.span,
            message,
            fix.into(),
        ));
    };
    public_key(text).map_err(|error| {
        let (code, message, fix) = match error {
            Error::Algorithm(algorithm) => (
                PUBLIC_KEY_ALGORITHM,
                format!(
                    "the public key is {algorithm:?}, and {noun} takes only \
                     `{ALGORITHM}`"
                ),
                algorithm_fix,
            ),
            Error::NotALine => (
                BAD_PUBLIC_KEY,
                "the public key is not the line of a `.pub` file".into(),
                fix,
            ),
            Error::Lines => (
                BAD_PUBLIC_KEY,
                "the public key is more than one line".into(),
                fix,
            ),
            Error::NotEd25519 => (
                BAD_PUBLIC_KEY,
                "the base64 of the public key is not an Ed25519 key".into(),
                fix,
            ),
            Error::SmallOrder(error) => (BAD_PUBLIC_KEY, error.to_string(), fix),
        };
        Diagnostic::new(code, value.span, message, fix.into())
    })
}

/// Why a text is not the line of an OpenSSH `.pub` file of an Ed25519 key.
enum Error {
    /// It has no algorithm and key, or its first word is no known algorithm.
    NotALine,
    /// The key is of another known algorithm, by its name.
    Algorithm(&'static str),
    /// It is more than one line.
    Lines,
    /// The base64 does not decode to an Ed25519 key, or is not the base64 that
    /// `ssh-key` writes for that key.
    NotEd25519,
    /// The key is a point of small order.
    SmallOrder(types::ed25519::SmallOrder),
}

/// Reads the line of an OpenSSH `.pub` file of an Ed25519 key: `ssh-ed25519`, the
/// base64 of the key, and a comment, which is optional and not kept. As OpenSSH reads
/// it, the comment is the rest of the line, so it may hold another key.
fn public_key(text: &str) -> Result<PublicKey, Error> {
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
    let written = key.to_openssh().unwrap_or_else(|_| {
        unreachable!("invariant: `ssh-key` writes each Ed25519 key that it reads")
    });
    if written != line {
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

/// The line of an OpenSSH `.pub` file of `key`, with no comment. A `node` block of
/// [`crate::profile::node`] holds it, and [`crate::profile::decode`] reads it back as
/// `key`.
#[must_use]
pub fn line(key: PublicKey) -> String {
    ssh_key::PublicKey::from(Ed25519PublicKey(key.to_bytes()))
        .to_openssh()
        .unwrap_or_else(|_| {
            unreachable!("invariant: `ssh-key` writes each Ed25519 key")
        })
}

/// Why a text is not the OpenSSH private key file of an Ed25519 key with no
/// passphrase. Its message quotes none of the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unreadable {
    /// It is not an OpenSSH private key file, which starts `-----BEGIN OPENSSH PRIVATE
    /// KEY-----`.
    Format,
    /// The key is of another algorithm: its name, or `ECDSA` for an ECDSA key.
    Algorithm(&'static str),
    /// The key has a passphrase.
    Passphrase,
    /// The public key of the file is not the key of its private key.
    Mismatch,
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format => f.write_str("the file is not an OpenSSH private key"),
            Self::Algorithm(algorithm) => write!(
                f,
                "the private key is {algorithm:?}, and a subject takes only \
                 `{ALGORITHM}`"
            ),
            Self::Passphrase => f.write_str("the private key has a passphrase"),
            Self::Mismatch => f.write_str(
                "the public key of the file is not the key of its private key",
            ),
        }
    }
}

/// Reads the OpenSSH private key file of an Ed25519 key with no passphrase, as
/// `ssh-keygen -t ed25519 -N ""` and [`key_file`] write it. It reads each such file
/// that OpenSSH reads, with a body of any width and any text after the end mark, and
/// also one with CRLF line ends or no line end after the end mark. The comment is not
/// kept.
///
/// # Errors
///
/// [`Unreadable`] says why the text is not such a file.
pub fn private_key(text: &str) -> Result<PrivateKey, Unreadable> {
    let file = match ssh_key::PrivateKey::from_openssh(lines(text)) {
        Ok(file) => file,
        // With no `ecdsa` feature, `ssh-key` reads no ECDSA private key. A `none` KDF
        // with options, which a corrupt file can hold, also gives this error.
        Err(ssh_key::Error::AlgorithmUnknown) => {
            return Err(Unreadable::Algorithm("ECDSA"));
        }
        Err(_) => return Err(Unreadable::Format),
    };
    let algorithm = file.algorithm();
    if algorithm != ssh_key::Algorithm::Ed25519 {
        let known = OTHER_ALGORITHMS
            .iter()
            .find(|other| **other == algorithm.as_str());
        return Err(
            known.map_or(Unreadable::Format, |other| Unreadable::Algorithm(other))
        );
    }
    if file.is_encrypted() {
        return Err(Unreadable::Passphrase);
    }
    let KeypairData::Ed25519(pair) = file.key_data() else {
        unreachable!("invariant: the key data of an Ed25519 key with no passphrase")
    };
    let key = PrivateKey(pair.private.to_bytes());
    if key.public().to_bytes() != pair.public.0 {
        return Err(Unreadable::Mismatch);
    }
    Ok(key)
}

/// `text` up to its end mark as `ssh-key` takes it: the body in lines of 70 characters,
/// and each line end as `\n`. `ssh-key` 0.6.7 reads only lines of 70, as `ssh-keygen`
/// writes them, and OpenSSH reads any width and skips the text after the end mark.
fn lines(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let end = lines.iter().position(|line| line.starts_with("-----END"));
    let Some([begin, body @ .., end]) = end.map(|end| &lines[..=end]) else {
        return text.to_owned();
    };
    let body: Vec<char> = body.concat().chars().collect();
    let mut file = format!("{begin}\n");
    for line in body.chunks(70) {
        file.extend(line);
        file.push('\n');
    }
    file.push_str(end);
    file.push('\n');
    file
}

/// The OpenSSH private key file of `key` with no passphrase and with `comment`, as
/// `ssh-keygen -t ed25519 -N ""` writes it. [`private_key`] reads it back as `key`.
#[must_use]
pub fn key_file(key: &PrivateKey, comment: &str) -> String {
    let pair = Ed25519Keypair {
        public: Ed25519PublicKey(key.public().to_bytes()),
        private: Ed25519PrivateKey::from_bytes(&key.0),
    };
    let file = ssh_key::PrivateKey::new(KeypairData::Ed25519(pair), comment)
        .unwrap_or_else(|_| {
            unreachable!("invariant: a key with no passphrase is made")
        });
    let text = file.to_openssh(LineEnding::LF).unwrap_or_else(|_| {
        unreachable!("invariant: `ssh-key` writes each Ed25519 key")
    });
    String::from(text.as_str())
}
