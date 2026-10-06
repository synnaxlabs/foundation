//! A POST request, checked once and written many times.

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
    /// - [`Error::Target`] for a target that is not origin-form.
    /// - [`Error::Host`] for a host that is not a host with an optional port.
    /// - [`Error::Name`] for a header name that is not a token.
    /// - [`Error::Value`] for a header value with a byte that is not visible
    ///   ASCII, a space, or a tab.
    /// - [`Error::Reserved`] for a header the client writes itself or never sends.
    /// - [`Error::Duplicate`] for a header name given twice, in any case.
    pub(crate) fn new(
        host: &str,
        target: &str,
        headers: &[(&str, &str)],
    ) -> Result<Self, Error> {
        if !target.starts_with('/')
            || !target.bytes().all(|b| b.is_ascii_graphic() && b != b'#')
        {
            return Err(Error::Target(target.into()));
        }
        if host.is_empty() || !host.bytes().all(authority) {
            return Err(Error::Host(host.into()));
        }
        let mut head = format!("POST {target} HTTP/1.1\r\n").into_bytes();
        header(&mut head, "Host", host)?;
        for (index, &(name, value)) in headers.iter().enumerate() {
            if !name.bytes().all(token) || name.is_empty() {
                return Err(Error::Name(name.into()));
            }
            if RESERVED.iter().any(|own| own.eq_ignore_ascii_case(name)) {
                return Err(Error::Reserved(name.into()));
            }
            let earlier = headers.iter().take(index);
            if earlier
                .into_iter()
                .any(|(seen, _)| seen.eq_ignore_ascii_case(name))
            {
                return Err(Error::Duplicate(name.into()));
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
    if !value
        .bytes()
        .all(|b| b.is_ascii_graphic() || b == b' ' || b == b'\t')
    {
        return Err(Error::Value(name.into()));
    }
    for part in [name, ": ", value, "\r\n"] {
        head.extend_from_slice(part.as_bytes());
    }
    Ok(())
}

/// Whether `byte` may be in a host with a port: a name, an IPv4 address, or a
/// bracketed IPv6 address, as RFC 3986 section 3.2.2 says.
fn authority(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:[]%".contains(&byte)
}

/// Whether `byte` may be in a token, as RFC 9110 section 5.6.2 says.
fn token(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte)
}

/// Why a request is not valid HTTP/1.1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// A request target that is not origin-form: it does not start with `/`, or
    /// holds `#` or a byte that is not visible ASCII.
    Target(String),
    /// A host that is not a host with an optional port.
    Host(String),
    /// A header name that is not a token.
    Name(String),
    /// A header value with a byte that is not visible ASCII, a space, or a tab, by
    /// its header name.
    Value(String),
    /// A header the client writes itself or never sends.
    Reserved(String),
    /// A header name given twice, in any case.
    Duplicate(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Target(target) => {
                write!(f, "the target {target:?} is not an origin-form target")
            }
            Self::Host(host) => {
                write!(f, "the host {host:?} is not a host with an optional port")
            }
            Self::Name(name) => write!(f, "the header name {name:?} is not a token"),
            Self::Value(name) => write!(
                f,
                "the value of the header {name:?} holds a byte that is not visible \
                 ASCII, a space, or a tab"
            ),
            Self::Reserved(name) => {
                write!(f, "the client does not take the header {name:?}")
            }
            Self::Duplicate(name) => write!(f, "the header {name:?} is given twice"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
