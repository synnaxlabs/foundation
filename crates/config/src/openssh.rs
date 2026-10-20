//! The OpenSSH forms of an Ed25519 key: the line of a `.pub` file, and the private key
//! file.

use std::fmt;

use base64ct::{Base64, Encoding};
use document::diagnostic::{Code, Diagnostic};
use document::value::{Kind, Value};
use ssh_key::private::{Ed25519Keypair, Ed25519PrivateKey, KeypairData};
use ssh_key::public::Ed25519PublicKey;
use ssh_key::{HashAlg, LineEnding};
use types::ed25519::{PrivateKey, PublicKey};

const BAD_PUBLIC_KEY: Code = Code::new("config.bad-public-key");
const PUBLIC_KEY_ALGORITHM: Code = Code::new("config.public-key-algorithm");
const KEY_FORMAT: Code = Code::new("config.key-format");
const KEY_ALGORITHM: Code = Code::new("config.key-algorithm");
const KEY_PASSPHRASE: Code = Code::new("config.key-passphrase");
const KEY_MISMATCH: Code = Code::new("config.key-mismatch");
const KEY_COMMENT: Code = Code::new("config.key-comment");
/// The fix of a key that a subject cannot take.
const NEW_KEY: &str = "Make a new key with `ssh-keygen -t ed25519 -N \"\"`, and add \
                       its `.pub` line to `keys` of the `subject` block";
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

/// Why a file is not the OpenSSH private key file of an Ed25519 key with no
/// passphrase. `Display` gives the message, which quotes none of the text: a lower-case
/// clause with no final period. [`Unreadable::code`] and [`Unreadable::fix`] give the
/// other parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unreadable {
    /// It is not an OpenSSH private key file as `ssh-keygen` writes it: its first line
    /// that starts `-----BEGIN ` at the start of the text or after a `\n` is not
    /// `-----BEGIN OPENSSH PRIVATE KEY-----`, the text before that line holds a NUL, no
    /// end mark follows that line, or it breaks the format in a way that OpenSSH reads,
    /// as [`private_key`] says.
    Format,
    /// The key is of another algorithm: its name, or `ECDSA` for an ECDSA key.
    Algorithm(&'static str),
    /// The key has a passphrase.
    Passphrase,
    /// The public key of the file is not the key of its private key.
    Mismatch,
    /// A text of the file, such as its comment, is not UTF-8.
    Comment,
}

impl Unreadable {
    /// The stable code of the error.
    #[must_use]
    pub const fn code(&self) -> Code {
        match self {
            Self::Format => KEY_FORMAT,
            Self::Algorithm(_) => KEY_ALGORITHM,
            Self::Passphrase => KEY_PASSPHRASE,
            Self::Mismatch => KEY_MISMATCH,
            Self::Comment => KEY_COMMENT,
        }
    }

    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(&self) -> &'static str {
        match self {
            Self::Format => {
                "Use the private key file that `ssh-keygen -t ed25519 -N \"\"` writes, \
                 not its `.pub` file"
            }
            Self::Algorithm(_) | Self::Mismatch => NEW_KEY,
            Self::Passphrase => {
                "Use a key file with no passphrase, as `ssh-keygen -t ed25519 -N \"\"` \
                 writes it"
            }
            Self::Comment => {
                "Give the key a comment in UTF-8 with `ssh-keygen -c -C <comment> -f \
                 <file>`"
            }
        }
    }
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format => f.write_str(
                "the file is not an OpenSSH private key as `ssh-keygen` writes it",
            ),
            Self::Algorithm(algorithm) => write!(
                f,
                "the private key is {algorithm:?}, and a subject takes only \
                 `{ALGORITHM}`"
            ),
            Self::Passphrase => f.write_str("the private key has a passphrase"),
            Self::Mismatch => f.write_str(
                "the public key of the file is not the key of its private key",
            ),
            Self::Comment => f.write_str(
                "the key file holds a text that is not UTF-8, such as its comment",
            ),
        }
    }
}

/// Reads the OpenSSH private key file of an Ed25519 key with no passphrase, as
/// `ssh-keygen -t ed25519 -N ""` and [`key_file`] write it. It reads each such file
/// that `ssh-key` 0.6.7 reads and that is UTF-8 up to its begin mark, also one with a
/// body of any width or with white space, any bytes on the lines after the end mark,
/// CRLF line ends, or no line end after the end mark. It refuses some files that OpenSSH reads: a comment that is not UTF-8
/// ([`Unreadable::Comment`]), and, as [`Unreadable::Format`], padding of a block or
/// more, options of the `none` KDF, and a NUL in the body. The comment is not kept.
///
/// # Errors
///
/// [`Unreadable`] says why `file` is not such a file. A byte up to the line end of the
/// end mark that is not UTF-8 gives [`Unreadable::Format`].
pub fn private_key(file: &[u8]) -> Result<PrivateKey, Unreadable> {
    let Some(text) = std::str::from_utf8(marked(file)).ok().and_then(lines) else {
        return Err(Unreadable::Format);
    };
    let file = match ssh_key::PrivateKey::from_openssh(&text) {
        Ok(file) => file,
        // With no `ecdsa` feature, `ssh-key` reads no ECDSA private key.
        Err(ssh_key::Error::AlgorithmUnknown) if ecdsa(&text) => {
            return Err(Unreadable::Algorithm("ECDSA"));
        }
        // The error that `ssh-key` gives for a text that is not UTF-8.
        Err(error)
            if String::from_utf8(vec![0xe9])
                .is_err_and(|text| error == text.utf8_error().into()) =>
        {
            return Err(Unreadable::Comment);
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

/// `file` up to the end of the line of the end mark that [`lines`] takes, or all of
/// `file` with no begin mark or no end mark.
fn marked(file: &[u8]) -> &[u8] {
    let begin = if file.starts_with(b"-----BEGIN ") {
        0
    } else {
        match file
            .windows(12)
            .position(|window| window == b"\n-----BEGIN ")
        {
            Some(newline) => newline + 1,
            None => return file,
        }
    };
    let mut start = begin;
    for line in file[begin..].split(|byte| *byte == b'\n' || *byte == b'\r') {
        start += line.len();
        if line.starts_with(b"-----END") {
            return &file[..start];
        }
        start += 1;
    }
    file
}

/// `text` from its begin mark to its end mark as `ssh-key` takes it: the body with no
/// white space, in lines of 70 characters, and each line end (`\n`, `\r\n`, or `\r`)
/// as `\n`. `ssh-key` 0.6.7 takes a begin mark only at the start of the text or after
/// a `\n`, skips the text before it when it holds no NUL, and reads only lines of 70,
/// as `ssh-keygen` writes them. OpenSSH reads any width, skips white space in the body
/// as C `isspace` finds it, and skips the text after the end mark. `None` when the text
/// before the begin mark holds a NUL, or no end mark follows a begin mark.
fn lines(text: &str) -> Option<String> {
    let start = if text.starts_with("-----BEGIN ") {
        0
    } else {
        text.find("\n-----BEGIN ")? + 1
    };
    if text[..start].contains('\0') {
        return None;
    }
    let mut lines = text[start..].split(['\n', '\r']);
    let begin = lines.next()?;
    let space = |c: &char| c.is_ascii_whitespace() || *c == '\x0b';
    let mut body = Vec::new();
    let end = loop {
        let line = lines.next()?;
        if line.starts_with("-----END") {
            break line;
        }
        body.extend(line.chars().filter(|c| !space(c)));
    };
    let mut file = format!("{begin}\n");
    for line in body.chunks(70) {
        file.extend(line);
        file.push('\n');
    }
    file.push_str(end);
    file.push('\n');
    Some(file)
}

/// Whether the public key of `file`, as [`lines`] gives it, is an ECDSA key.
fn ecdsa(file: &str) -> bool {
    let body: String = file
        .lines()
        .filter(|line| !line.starts_with("-----"))
        .collect();
    let binary = Base64::decode_vec(&body).unwrap_or_default();
    algorithm(&binary).is_some_and(|name| {
        name.starts_with(b"ecdsa-sha2-") || name.starts_with(b"sk-ecdsa-")
    })
}

/// The algorithm of the public key of the binary form of a private key file.
fn algorithm(binary: &[u8]) -> Option<&[u8]> {
    let mut rest = binary.strip_prefix(b"openssh-key-v1\0")?;
    // The cipher, the KDF, and its options.
    for _ in 0..3 {
        field(&mut rest)?;
    }
    let mut keys = rest.get(4..)?;
    let mut public = field(&mut keys)?;
    field(&mut public)
}

/// The next string of `rest`, with its 4-byte length before it.
fn field<'a>(rest: &mut &'a [u8]) -> Option<&'a [u8]> {
    let (length, tail) = rest.split_first_chunk::<4>()?;
    let length = usize::try_from(u32::from_be_bytes(*length)).ok()?;
    let (field, tail) = tail.split_at_checked(length)?;
    *rest = tail;
    Some(field)
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
