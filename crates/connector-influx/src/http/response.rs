//! Reads an HTTP/1.1 response, as RFC 9112 says.

use std::borrow::Cow;
use std::fmt;

use httparse::{EMPTY_HEADER, Header, Status};

/// The most header fields a head or a trailer section may hold.
const HEADERS: usize = 32;

/// One final response, found at the front of the bytes the stream gave.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Response<'a> {
    /// The status code.
    pub(crate) status: u16,
    /// The body, without framing.
    pub(crate) body: Cow<'a, [u8]>,
    /// The bytes it took, interim responses included. The next response starts
    /// after them.
    pub(crate) used: usize,
    /// The server closes the stream after this response.
    pub(crate) last: bool,
}

/// Reads the response at the front of `bytes`. `closed` says that no more bytes
/// come. Gives `None` when more must be read. Skips each interim (1xx) response.
///
/// # Errors
///
/// - [`Error::Head`] for a head that is not valid or holds more than 32 fields.
/// - [`Error::Version`] for a response that is not HTTP/1.1.
/// - [`Error::Switch`] for a 101: the client never asks to switch protocols.
/// - [`Error::Ambiguous`], [`Error::Length`], or [`Error::Encoding`] for a
///   framing that is not one known length, chunked, or the close.
/// - [`Error::Chunk`] for a chunked body that is not valid.
/// - [`Error::Oversize`] for a response that takes more than `cap` bytes.
/// - [`Error::Truncated`] for a stream that closed before the response ended.
pub(crate) fn read(
    bytes: &[u8],
    closed: bool,
    cap: usize,
) -> Result<Option<Response<'_>>, Error> {
    let mut start = 0;
    loop {
        let rest = bytes.get(start..).unwrap_or_default();
        let mut fields = [EMPTY_HEADER; HEADERS];
        let mut head = httparse::Response::new(&mut fields);
        let size = match head.parse(rest).map_err(Error::Head)? {
            Status::Complete(size) => size,
            Status::Partial => return more(bytes.len(), closed, cap),
        };
        if head.version != Some(1) {
            return Err(Error::Version);
        }
        let status = head.code.expect("invariant: a whole head has a status");
        let framing = Framing::new(status, head.headers)?;
        let end = start.checked_add(size).filter(|&end| end <= cap);
        let end = end.ok_or(Error::Oversize { cap })?;
        if framing == Framing::Interim {
            start = end;
            continue;
        }
        let Some(body) = framing.body(bytes, end, closed, cap)? else {
            return Ok(None);
        };
        let last = framing == Framing::Close || closing(head.headers);
        return Ok(Some(Response {
            status,
            body: body.bytes,
            used: body.end,
            last,
        }));
    }
}

/// Whether `headers` say the server closes the stream after this response.
fn closing(headers: &[Header<'_>]) -> bool {
    headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("Connection"))
        .flat_map(|header| header.value.split(|&b| b == b','))
        .any(|option| option.trim_ascii().eq_ignore_ascii_case(b"close"))
}

/// The result when the response needs bytes past the `got` that came.
fn more<T>(got: usize, closed: bool, cap: usize) -> Result<Option<T>, Error> {
    if got > cap {
        Err(Error::Oversize { cap })
    } else if closed {
        Err(Error::Truncated)
    } else {
        Ok(None)
    }
}

/// How the body of a response ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Framing {
    /// An interim (1xx) response with no body. The final response follows.
    Interim,
    /// A final response with no body.
    Empty,
    /// A body of this many bytes.
    Length(u64),
    /// A chunked body.
    Chunked,
    /// A body that ends when the stream closes.
    Close,
}

/// One response body, found after its head.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Body<'a> {
    /// The body, without framing.
    bytes: Cow<'a, [u8]>,
    /// Where the body ends in the stream, framing included.
    end: usize,
}

impl Framing {
    /// The framing of a response with `status` and `headers`, as RFC 9112 section
    /// 6.3 says.
    fn new(status: u16, headers: &[Header<'_>]) -> Result<Self, Error> {
        let find = |want: &'static str| -> Vec<&[u8]> {
            headers
                .iter()
                .filter(|header| header.name.eq_ignore_ascii_case(want))
                .map(|header| header.value)
                .collect()
        };
        let lengths = find("Content-Length");
        let encodings = find("Transfer-Encoding");
        if !lengths.is_empty() && !encodings.is_empty() {
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
        match (lengths.as_slice(), encodings.as_slice()) {
            ([], []) => Ok(Self::Close),
            ([], [encoding]) if encoding.eq_ignore_ascii_case(b"chunked") => {
                Ok(Self::Chunked)
            }
            ([], encodings) => Err(Error::Encoding(lossy(&encodings.join(&b", "[..])))),
            ([length], []) => decimal(length)
                .map(Self::Length)
                .ok_or_else(|| Error::Length(lossy(length))),
            (lengths, _) => Err(Error::Length(lossy(&lengths.join(&b", "[..])))),
        }
    }

    /// Finds the body that starts at `start` in `bytes`, all of which the stream
    /// gave, so that it ends at most `cap` bytes into the stream.
    fn body(
        self,
        bytes: &[u8],
        start: usize,
        closed: bool,
        cap: usize,
    ) -> Result<Option<Body<'_>>, Error> {
        let span = |size: usize| {
            let end = start.checked_add(size).filter(|&end| end <= cap);
            end.ok_or(Error::Oversize { cap })
        };
        let whole = |end: usize| {
            let body = bytes.get(start..end)?;
            Some(Body {
                bytes: Cow::Borrowed(body),
                end,
            })
        };
        match self {
            Self::Interim | Self::Empty => Ok(whole(start)),
            Self::Length(size) => {
                let end = span(usize::try_from(size).unwrap_or(usize::MAX))?;
                whole(end).map_or_else(
                    || more(bytes.len(), closed, cap),
                    |body| Ok(Some(body)),
                )
            }
            Self::Chunked => chunked(bytes, start, closed, cap),
            Self::Close if bytes.len() > cap => Err(Error::Oversize { cap }),
            Self::Close => Ok(closed.then(|| whole(bytes.len())).flatten()),
        }
    }
}

/// Reads the chunked body that starts at `start` in `bytes`, with its trailer
/// section, as RFC 9112 section 7.1 says. Drops chunk extensions and trailers.
fn chunked(
    bytes: &[u8],
    start: usize,
    closed: bool,
    cap: usize,
) -> Result<Option<Body<'_>>, Error> {
    let mut body = Vec::new();
    let mut at = start;
    loop {
        let rest = bytes.get(at..).unwrap_or_default();
        if rest.first().is_some_and(|b| !b.is_ascii_hexdigit()) {
            return Err(Error::Chunk);
        }
        let (line, size) = match httparse::parse_chunk_size(rest) {
            Ok(Status::Complete(found)) => found,
            Ok(Status::Partial) => return more(bytes.len(), closed, cap),
            Err(httparse::InvalidChunkSize) => return Err(Error::Chunk),
        };
        at = at.checked_add(line).ok_or(Error::Chunk)?;
        if size == 0 {
            let rest = bytes.get(at..).unwrap_or_default();
            let mut fields = [EMPTY_HEADER; HEADERS];
            return match httparse::parse_headers(rest, &mut fields) {
                Ok(Status::Complete((size, _))) => {
                    let end = at.checked_add(size).filter(|&end| end <= cap);
                    Ok(Some(Body {
                        bytes: Cow::Owned(body),
                        end: end.ok_or(Error::Oversize { cap })?,
                    }))
                }
                Ok(Status::Partial) => more(bytes.len(), closed, cap),
                Err(_) => Err(Error::Chunk),
            };
        }
        let end = usize::try_from(size)
            .ok()
            .and_then(|size| at.checked_add(size)?.checked_add(2))
            .filter(|&end| end <= cap)
            .ok_or(Error::Oversize { cap })?;
        let Some(chunk) = bytes.get(at..end) else {
            return more(bytes.len(), closed, cap);
        };
        match chunk.split_last_chunk() {
            Some((data, b"\r\n")) => body.extend_from_slice(data),
            _ => return Err(Error::Chunk),
        }
        at = end;
    }
}

/// The value of a Content-Length: one decimal number.
fn decimal(value: &[u8]) -> Option<u64> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

/// `bytes` as text, with each byte that is not UTF-8 replaced.
fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Why a response is not valid HTTP/1.1 for this client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// A head that is not valid, or holds more than 32 fields.
    Head(httparse::Error),
    /// A response that is not HTTP/1.1.
    Version,
    /// A 101 response, to a client that never asks to switch protocols.
    Switch,
    /// A response with both Content-Length and Transfer-Encoding.
    Ambiguous,
    /// A Content-Length that is not one decimal number, with each value.
    Length(String),
    /// A Transfer-Encoding that is not `chunked` alone, with each value.
    Encoding(String),
    /// A chunked body with a bad chunk size, line end, or trailer.
    Chunk,
    /// A response that takes more than the cap.
    Oversize {
        /// The cap, in bytes.
        cap: usize,
    },
    /// A stream that closed before the response ended.
    Truncated,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Head(error) => {
                write!(f, "a response head that is not valid: {error}")
            }
            Self::Version => write!(f, "a response that is not HTTP/1.1"),
            Self::Switch => {
                write!(f, "a 101 response to a client that asked for no switch")
            }
            Self::Ambiguous => write!(
                f,
                "a response with both Content-Length and Transfer-Encoding"
            ),
            Self::Length(length) => {
                write!(f, "the Content-Length {length:?} is not one decimal number")
            }
            Self::Encoding(encoding) => {
                write!(f, "the Transfer-Encoding {encoding:?} is not supported")
            }
            Self::Chunk => write!(f, "a chunked body that is not valid"),
            Self::Oversize { cap } => {
                write!(f, "a response over the cap of {cap} bytes")
            }
            Self::Truncated => write!(f, "the stream closed before the response ended"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
