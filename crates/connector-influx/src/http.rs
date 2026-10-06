//! HTTP/1.1, sans I/O: a POST request and the framing of its response body.

use std::fmt;
use std::io::Write as _;

/// One POST target and its headers, checked once.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Request {
    target: String,
    /// The head up to Content-Length.
    head: Vec<u8>,
}

/// Headers the client writes itself, or never sends.
const RESERVED: [&str; 4] = ["Host", "Content-Length", "Transfer-Encoding", "Expect"];

impl Request {
    /// Checks `host`, `target`, and `headers`.
    ///
    /// # Errors
    ///
    /// - [`Error::Target`] for a target that does not start with `/` or holds a
    ///   space or a control character.
    /// - [`Error::Name`] for a header name that is not a token.
    /// - [`Error::Value`] for a host or header value with a control character other
    ///   than a tab.
    /// - [`Error::Reserved`] for a header the client writes itself or never sends.
    pub(crate) fn new(
        host: &str,
        target: &str,
        headers: &[(&str, &str)],
    ) -> Result<Self, Error> {
        if !target.starts_with('/') || target.bytes().any(|b| b <= b' ' || b == 0x7F) {
            return Err(Error::Target(target.into()));
        }
        let mut head = format!("POST {target} HTTP/1.1\r\n").into_bytes();
        header(&mut head, "Host", host)?;
        for &(name, value) in headers {
            if !name.bytes().all(token) || name.is_empty() {
                return Err(Error::Name(name.into()));
            }
            if RESERVED.iter().any(|own| own.eq_ignore_ascii_case(name)) {
                return Err(Error::Reserved(name.into()));
            }
            header(&mut head, name, value)?;
        }
        Ok(Self {
            target: target.into(),
            head,
        })
    }

    /// Appends the request with `body` to `out`.
    pub(crate) fn write(&self, out: &mut Vec<u8>, body: &[u8]) {
        out.extend_from_slice(&self.head);
        write!(out, "Content-Length: {}\r\n\r\n", body.len())
            .expect("invariant: a write to a Vec never fails");
        out.extend_from_slice(body);
    }
}

/// Shows only the target: a header may hold a secret.
impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// Appends the header `name` with `value` to `head`.
fn header(head: &mut Vec<u8>, name: &str, value: &str) -> Result<(), Error> {
    if value.bytes().any(|b| (b < b' ' && b != b'\t') || b == 0x7F) {
        return Err(Error::Value(name.into()));
    }
    for part in [name, ": ", value, "\r\n"] {
        head.extend_from_slice(part.as_bytes());
    }
    Ok(())
}

/// Whether `byte` may be in a token, as RFC 9110 section 5.6.2 says.
fn token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// How the body of a response ends, as RFC 9112 section 6.3 says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Framing {
    /// No body.
    Empty,
    /// A body of this many bytes.
    Length(u64),
    /// A body that ends when the stream closes.
    Close,
}

impl Framing {
    /// The framing of a response with `status` and `headers`. A 1xx, 204, or 304
    /// response has no body.
    ///
    /// # Errors
    ///
    /// - [`Error::Both`] for Content-Length with Transfer-Encoding.
    /// - [`Error::Length`] for a Content-Length that is not one decimal number.
    /// - [`Error::Encoding`] for any Transfer-Encoding.
    pub(crate) fn new(status: u16, headers: &[(&str, &[u8])]) -> Result<Self, Error> {
        let find = |want: &str| {
            let mut found = headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case(want));
            (
                found.next().map(|&(_, value)| value),
                found.next().is_some(),
            )
        };
        let (length, lengths) = find("Content-Length");
        let (encoding, _) = find("Transfer-Encoding");
        if length.is_some() && encoding.is_some() {
            return Err(Error::Both);
        }
        if (100..200).contains(&status) || status == 204 || status == 304 {
            return Ok(Self::Empty);
        }
        if let Some(encoding) = encoding {
            return Err(Error::Encoding(lossy(encoding)));
        }
        let Some(length) = length else {
            return Ok(Self::Close);
        };
        let number =
            (!lengths && !length.is_empty() && length.iter().all(u8::is_ascii_digit))
                .then(|| std::str::from_utf8(length).ok()?.parse().ok())
                .flatten();
        number
            .map(Self::Length)
            .ok_or_else(|| Error::Length(lossy(length)))
    }

    /// Finds the body at the front of `bytes`, the bytes after the head. `closed`
    /// says that no more bytes come. Gives the body, or `None` when more must be
    /// read.
    ///
    /// # Errors
    ///
    /// - [`Error::Large`] for a body over `cap` bytes.
    /// - [`Error::Truncated`] for a stream that closed before the body ended.
    pub(crate) fn body(
        self,
        bytes: &[u8],
        closed: bool,
        cap: usize,
    ) -> Result<Option<&[u8]>, Error> {
        match self {
            Self::Empty => Ok(Some(&[])),
            Self::Length(want) => {
                let size = usize::try_from(want)
                    .ok()
                    .filter(|&size| size <= cap)
                    .ok_or(Error::Large { cap })?;
                match bytes.get(..size) {
                    Some(body) => Ok(Some(body)),
                    None if closed => Err(Error::Truncated {
                        want,
                        got: bytes.len(),
                    }),
                    None => Ok(None),
                }
            }
            Self::Close if bytes.len() > cap => Err(Error::Large { cap }),
            Self::Close => Ok(closed.then_some(bytes)),
        }
    }
}

/// `bytes` as text, with each byte that is not UTF-8 replaced.
fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Why a request or a response is not valid HTTP/1.1 for this client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// A request target that does not start with `/` or holds a space or a control
    /// character.
    Target(String),
    /// A header name that is not a token.
    Name(String),
    /// A host or header value with a control character, by its header name.
    Value(String),
    /// A header the client writes itself or never sends.
    Reserved(String),
    /// A response with both Content-Length and Transfer-Encoding.
    Both,
    /// A Content-Length that is not one decimal number.
    Length(String),
    /// A Transfer-Encoding.
    Encoding(String),
    /// A response body over the cap.
    Large {
        /// The cap, in bytes.
        cap: usize,
    },
    /// A stream that closed before the body ended.
    Truncated {
        /// The body length.
        want: u64,
        /// The bytes that came.
        got: usize,
    },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Target(target) => write!(
                f,
                "the target {target:?} does not start with '/' or holds a space or a \
                 control character"
            ),
            Self::Name(name) => write!(f, "the header name {name:?} is not a token"),
            Self::Value(name) => {
                write!(
                    f,
                    "the value of the header {name:?} holds a control character"
                )
            }
            Self::Reserved(name) => {
                write!(f, "the client does not take the header {name:?}")
            }
            Self::Both => write!(
                f,
                "a response with both Content-Length and Transfer-Encoding"
            ),
            Self::Length(length) => {
                write!(f, "the Content-Length {length:?} is not one decimal number")
            }
            Self::Encoding(encoding) => {
                write!(f, "the Transfer-Encoding {encoding:?} is not supported")
            }
            Self::Large { cap } => {
                write!(f, "a response body over the cap of {cap} bytes")
            }
            Self::Truncated { want, got } => {
                write!(f, "the stream closed after {got} of {want} body bytes")
            }
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
