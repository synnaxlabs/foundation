//! The framing of a response body, as RFC 9112 section 6.3 says.

use std::borrow::Cow;
use std::fmt;

/// How the body of a response ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Framing {
    /// An interim (1xx) response with no body. The caller reads the next head: the
    /// final response follows.
    Interim,
    /// A final response with no body.
    Empty,
    /// A body of this many bytes.
    Length(u64),
    /// A body that ends when the stream closes.
    Close,
}

/// One response body, found at the front of the bytes after its head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Body<'a> {
    /// The body, without framing.
    pub(crate) bytes: Cow<'a, [u8]>,
    /// The bytes it took, framing included. The next response starts after them.
    pub(crate) used: usize,
}

impl<'a> Body<'a> {
    /// A body that took exactly its own bytes: no framing inside it.
    fn whole(bytes: &'a [u8]) -> Self {
        Self {
            bytes: Cow::Borrowed(bytes),
            used: bytes.len(),
        }
    }
}

impl Framing {
    /// The framing of a response with `status` and `headers`. A 1xx, 204, or 304
    /// response has no body.
    ///
    /// # Errors
    ///
    /// - [`Error::Ambiguous`] for Content-Length with Transfer-Encoding, on any
    ///   status.
    /// - [`Error::Switch`] for a 101: the client never asks to switch protocols.
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
            return Err(Error::Ambiguous);
        }
        if status == 101 {
            return Err(Error::Switch);
        }
        if (100..200).contains(&status) {
            return Ok(Self::Interim);
        }
        if status == 204 || status == 304 {
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
    /// says that no more bytes come. Gives `None` when more must be read.
    ///
    /// # Errors
    ///
    /// - [`Error::Oversize`] for a body over `cap` bytes.
    /// - [`Error::Truncated`] for a stream that closed before the body ended.
    pub(crate) fn body(
        self,
        bytes: &[u8],
        closed: bool,
        cap: usize,
    ) -> Result<Option<Body<'_>>, Error> {
        match self {
            Self::Interim | Self::Empty => Ok(Some(Body::whole(&[]))),
            Self::Length(want) => {
                let size = usize::try_from(want)
                    .ok()
                    .filter(|&size| size <= cap)
                    .ok_or(Error::Oversize { cap })?;
                match bytes.get(..size) {
                    Some(body) => Ok(Some(Body::whole(body))),
                    None if closed => Err(Error::Truncated {
                        want,
                        got: bytes.len(),
                    }),
                    None => Ok(None),
                }
            }
            Self::Close if bytes.len() > cap => Err(Error::Oversize { cap }),
            Self::Close => Ok(closed.then(|| Body::whole(bytes))),
        }
    }
}

/// `bytes` as text, with each byte that is not UTF-8 replaced.
fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Why a response is not valid HTTP/1.1 for this client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// A response with both Content-Length and Transfer-Encoding.
    Ambiguous,
    /// A 101 response, to a client that never asks to switch protocols.
    Switch,
    /// A Content-Length that is not one decimal number.
    Length(String),
    /// A Transfer-Encoding.
    Encoding(String),
    /// A response body over the cap.
    Oversize {
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
            Self::Ambiguous => write!(
                f,
                "a response with both Content-Length and Transfer-Encoding"
            ),
            Self::Switch => {
                write!(f, "a 101 response to a client that asked for no switch")
            }
            Self::Length(length) => {
                write!(f, "the Content-Length {length:?} is not one decimal number")
            }
            Self::Encoding(encoding) => {
                write!(f, "the Transfer-Encoding {encoding:?} is not supported")
            }
            Self::Oversize { cap } => {
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
