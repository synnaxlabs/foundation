//! Modbus TCP: the MBAP header before each PDU, a client, and a device server.

mod client;
mod serve;

pub use client::{Client, Failure};
pub use serve::serve;

use crate::Error;
use crate::pdu::Request;

const HEADER: usize = 7;

/// The MBAP fields that a caller chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// Pairs a reply with its request.
    pub transaction: u16,
    /// The device behind a gateway, or 255 for a device on TCP itself.
    pub unit: u8,
}

/// One frame, read in place from the front of a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    /// The frame's header.
    pub header: Header,
    /// The frame's PDU.
    pub pdu: &'a [u8],
    /// The length of the whole frame, header included.
    pub len: usize,
}

/// Appends `request` to `out` as one Modbus TCP frame.
///
/// # Errors
///
/// As [`Request::encode`]. `out` is unchanged then.
pub fn encode(
    header: Header,
    request: &Request,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    request.size()?;
    let start = open(header, out);
    request.write_to(out);
    seal(out, start);
    Ok(())
}

/// Appends the MBAP header of a frame, with its length left to [`seal`], and gives
/// where the frame starts in `out`.
fn open(header: Header, out: &mut Vec<u8>) -> usize {
    let start = out.len();
    out.extend_from_slice(&header.transaction.to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, header.unit]);
    start
}

/// Fills in the length of the frame that starts at `start` in `out` and runs to its
/// end.
///
/// # Panics
///
/// Only on a bug in this crate: no header at `start`, or a PDU longer than 253
/// bytes.
fn seal(out: &mut [u8], start: usize) {
    let (head, pdu) = out
        .get_mut(start..)
        .and_then(|frame| frame.split_first_chunk_mut::<HEADER>())
        .expect("invariant: `open` wrote a header at `start`");
    let length = pdu
        .len()
        .checked_add(1)
        .and_then(|n| u16::try_from(n).ok())
        .expect("invariant: a PDU is at most 253 bytes");
    let [_, _, _, _, high, low, _] = head;
    [*high, *low] = length.to_be_bytes();
}

/// Reads the first frame in `bytes`, or `None` when `bytes` holds less than one
/// frame.
///
/// # Errors
///
/// [`Error::Protocol`] or [`Error::Length`]. The stream is out of step then.
pub fn decode(bytes: &[u8]) -> Result<Option<Frame<'_>>, Error> {
    let Some(([t0, t1, p0, p1, l0, l1, unit], rest)) =
        bytes.split_first_chunk::<HEADER>()
    else {
        return Ok(None);
    };
    let protocol = u16::from_be_bytes([*p0, *p1]);
    if protocol != 0 {
        return Err(Error::Protocol(protocol));
    }
    let length = u16::from_be_bytes([*l0, *l1]);
    if !(2..=254).contains(&length) {
        return Err(Error::Length(length));
    }
    let size = usize::from(length).saturating_sub(1);
    let Some(pdu) = rest.get(..size) else {
        return Ok(None);
    };
    Ok(Some(Frame {
        header: Header {
            transaction: u16::from_be_bytes([*t0, *t1]),
            unit: *unit,
        },
        pdu,
        len: size.saturating_add(HEADER),
    }))
}

#[cfg(test)]
mod tests {
    mod network;

    use super::*;
    use crate::pdu::Table;

    const READ: Request = Request::Read {
        table: Table::HoldingRegisters,
        start: 107,
        count: 3,
    };

    const HEADER: Header = Header {
        transaction: 0x0102,
        unit: 0x11,
    };

    #[test]
    fn frames_a_request() {
        let mut out = vec![9];
        encode(HEADER, &READ, &mut out).expect("valid");
        let frame = [
            9, 0x01, 0x02, 0, 0, 0, 6, 0x11, 0x03, 0x00, 0x6B, 0x00, 0x03,
        ];
        assert_eq!(out, frame);
        let bad = Request::Read {
            table: Table::Coils,
            start: 0,
            count: 0,
        };
        assert_eq!(
            encode(HEADER, &bad, &mut out),
            Err(Error::Count {
                count: 0,
                max: 2000
            })
        );
        assert_eq!(out, frame, "out is unchanged");
    }

    #[test]
    fn reads_a_frame_only_when_it_is_whole() {
        let mut bytes = Vec::new();
        encode(HEADER, &READ, &mut bytes).expect("valid");
        let second = Header {
            transaction: 7,
            unit: 255,
        };
        encode(second, &READ, &mut bytes).expect("valid");
        for end in 0..12 {
            assert_eq!(decode(bytes.get(..end).expect("in bounds")), Ok(None));
        }
        let first = decode(&bytes).expect("valid").expect("whole");
        assert_eq!(
            first,
            Frame {
                header: HEADER,
                pdu: &[0x03, 0x00, 0x6B, 0x00, 0x03],
                len: 12,
            }
        );
        let rest = bytes.get(first.len..).expect("in bounds");
        let next = decode(rest).expect("valid").expect("whole");
        assert_eq!((next.header, next.len), (second, 12));
    }

    #[test]
    fn refuses_a_header_that_is_not_modbus() {
        let cases: [([u8; 7], Error, &str); 3] = [
            (
                [0, 1, 0, 1, 0, 6, 1],
                Error::Protocol(1),
                "an MBAP header with protocol 1, not 0",
            ),
            (
                [0, 1, 0, 0, 0, 1, 1],
                Error::Length(1),
                "an MBAP length of 1, outside 2 to 254",
            ),
            (
                [0, 1, 0, 0, 0, 255, 1],
                Error::Length(255),
                "an MBAP length of 255, outside 2 to 254",
            ),
        ];
        for (header, error, message) in cases {
            assert_eq!(decode(&header), Err(error.clone()));
            assert_eq!(error.to_string(), message);
        }
        let mut longest = vec![0, 1, 0, 0, 0, 254, 1];
        longest.resize(260, 0);
        let frame = decode(&longest).expect("valid").expect("whole");
        assert_eq!((frame.pdu.len(), frame.len), (253, 260));
        let shortest = [0, 1, 0, 0, 0, 2, 1, 0x07];
        let frame = decode(&shortest).expect("valid").expect("whole");
        assert_eq!((frame.pdu, frame.len), (&[0x07][..], 8));
    }
}
