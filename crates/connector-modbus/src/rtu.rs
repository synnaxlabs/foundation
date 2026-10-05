//! Modbus RTU framing: a unit address before each PDU, and a CRC-16 after it.
//!
//! A frame's length comes from its first bytes, so a frame reads as soon as its
//! last byte arrives. Silence on the line marks no boundary here.

use crate::Error;
use crate::pdu::{self, Request};

/// One frame, read in place from the front of a buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    /// The device's address on the line, or 0 for a broadcast.
    pub unit: u8,
    /// The frame's PDU.
    pub pdu: &'a [u8],
    /// The length of the whole frame, address and CRC included.
    pub len: usize,
}

/// Appends `request` to `out` as one RTU frame.
///
/// # Errors
///
/// As [`Request::encode`]. `out` is unchanged then.
pub fn encode(unit: u8, request: &Request, out: &mut Vec<u8>) -> Result<(), Error> {
    request.size()?;
    let start = out.len();
    out.push(unit);
    request.write_to(out);
    let crc = out
        .iter()
        .skip(start)
        .fold(INIT, |crc, &byte| step(crc, byte));
    out.extend_from_slice(&crc.to_le_bytes());
    Ok(())
}

/// Reads the first request frame in `bytes`, as a device does, or `None` when
/// `bytes` holds less than one frame.
///
/// # Errors
///
/// [`Error::Function`] or [`Error::Crc`]. The caller drops its bytes up to the
/// next silence on the line then.
pub fn decode_request(bytes: &[u8]) -> Result<Option<Frame<'_>>, Error> {
    decode(bytes, pdu::request_len)
}

/// Reads the first reply frame in `bytes`, or `None` when `bytes` holds less than
/// one frame.
///
/// # Errors
///
/// As [`decode_request`].
pub fn decode_reply(bytes: &[u8]) -> Result<Option<Frame<'_>>, Error> {
    decode(bytes, pdu::reply_len)
}

fn decode(
    bytes: &[u8],
    pdu_len: fn(&[u8]) -> Result<Option<usize>, Error>,
) -> Result<Option<Frame<'_>>, Error> {
    let Some((&unit, rest)) = bytes.split_first() else {
        return Ok(None);
    };
    let Some(size) = pdu_len(rest)? else {
        return Ok(None);
    };
    let Some((pdu, tail)) = rest.split_at_checked(size) else {
        return Ok(None);
    };
    let Some(&field) = tail.first_chunk::<2>() else {
        return Ok(None);
    };
    let want = pdu
        .iter()
        .fold(step(INIT, unit), |crc, &byte| step(crc, byte));
    let got = u16::from_le_bytes(field);
    if got != want {
        return Err(Error::Crc { want, got });
    }
    Ok(Some(Frame {
        unit,
        pdu,
        len: size.saturating_add(3),
    }))
}

const INIT: u16 = 0xFFFF;
const POLYNOMIAL: u16 = 0xA001;
const TABLE: [u16; 256] = table();

/// The CRC after `byte`, from the CRC before it.
fn step(crc: u16, byte: u8) -> u16 {
    let [low, _] = crc.to_le_bytes();
    let entry = TABLE
        .get(usize::from(low ^ byte))
        .expect("invariant: a byte indexes a table of 256");
    crc >> 8 ^ entry
}

/// The CRC step for each value of the low byte, shifted out bit by bit.
#[expect(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "a panic in const evaluation fails the build"
)]
const fn table() -> [u16; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    let mut crc: u16 = 0;
    while index < table.len() {
        let mut entry = crc;
        let mut bit = 0;
        while bit < 8 {
            entry = if entry & 1 == 1 {
                entry >> 1 ^ POLYNOMIAL
            } else {
                entry >> 1
            };
            bit += 1;
        }
        table[index] = entry;
        index += 1;
        crc = crc.wrapping_add(1);
    }
    table
}

#[cfg(test)]
mod tests;
