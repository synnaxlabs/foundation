mod bus;

use proptest::prelude::*;

use super::*;
use crate::pdu::{Reply, Table};

/// The CRC of the serial line specification, one bit at a time.
fn bitwise(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0xFFFF, |crc, &byte| {
        (0..8).fold(crc ^ u16::from(byte), |crc, _| {
            if crc & 1 == 1 {
                crc >> 1 ^ 0xA001
            } else {
                crc >> 1
            }
        })
    })
}

/// `bytes` with their CRC after them.
fn framed(bytes: &[u8]) -> Vec<u8> {
    let mut frame = bytes.to_vec();
    frame.extend_from_slice(&bitwise(bytes).to_le_bytes());
    frame
}

fn encoded(unit: u8, request: &Request) -> Vec<u8> {
    let mut out = Vec::new();
    encode(unit, request, &mut out).expect("valid");
    out
}

type Decoded<'a> = Result<Option<Frame<'a>>, Error>;

const READ: Request = Request::Read {
    table: Table::HoldingRegisters,
    start: 107,
    count: 3,
};

#[test]
fn matches_the_examples_of_the_specification() {
    assert_eq!(step(step(INIT, 0x02), 0x07), 0x1241);
    assert_eq!(bitwise(&[0x02, 0x07]), 0x1241);

    let mut out = vec![9];
    encode(0x11, &READ, &mut out).expect("valid");
    let request = [9, 0x11, 0x03, 0x00, 0x6B, 0x00, 0x03, 0x76, 0x87];
    assert_eq!(out, request);
    let bad = Request::Read {
        table: Table::Coils,
        start: 0,
        count: 0,
    };
    assert_eq!(
        encode(0x11, &bad, &mut out),
        Err(Error::Count {
            count: 0,
            max: 2000
        })
    );
    assert_eq!(out, request, "out is unchanged");
    assert_eq!(
        decode_request(request.get(1..).expect("in bounds")),
        Ok(Some(Frame {
            unit: 0x11,
            pdu: &[0x03, 0x00, 0x6B, 0x00, 0x03],
            len: 8,
        }))
    );

    let reply = [
        0x11, 0x03, 0x06, 0x02, 0x2B, 0x00, 0x00, 0x00, 0x64, 0xC8, 0xBA,
    ];
    let frame = decode_reply(&READ, &reply).expect("valid").expect("whole");
    assert_eq!((frame.unit, frame.len), (0x11, 11));
    let Ok(Reply::Registers(registers)) = READ.decode_reply(frame.pdu) else {
        panic!("registers");
    };
    assert_eq!(registers.iter().collect::<Vec<_>>(), [0x022B, 0, 0x64]);
}

#[test]
fn reads_a_frame_only_when_it_is_whole() {
    let requests = [
        encoded(1, &READ),
        encoded(
            2,
            &Request::WriteCoil {
                address: 172,
                value: true,
            },
        ),
        encoded(
            3,
            &Request::WriteCoils {
                start: 19,
                values: vec![true; 10],
            },
        ),
        encoded(
            4,
            &Request::WriteRegisters {
                start: 1,
                values: vec![0x000A, 0x0102],
            },
        ),
    ];
    for frame in requests {
        whole(&frame, decode_request);
    }
    let coils = Request::Read {
        table: Table::Coils,
        start: 0,
        count: 3,
    };
    let replies = [
        (
            READ,
            framed(&[1, 0x03, 0x06, 0x02, 0x2B, 0x00, 0x00, 0x00, 0x64]),
        ),
        (coils, framed(&[2, 0x01, 0x01, 0x05])),
        (
            Request::WriteCoils {
                start: 19,
                values: vec![true; 10],
            },
            framed(&[3, 0x0F, 0x00, 0x13, 0x00, 0x0A]),
        ),
        (READ, framed(&[4, 0x83, 0x02])),
    ];
    for (request, frame) in replies {
        whole(&frame, |bytes| decode_reply(&request, bytes));
    }
}

fn whole(frame: &[u8], decode: impl Fn(&[u8]) -> Decoded<'_>) {
    for end in 0..frame.len() {
        let prefix = frame.get(..end).expect("in bounds");
        assert_eq!(decode(prefix), Ok(None), "{prefix:x?}");
    }
    let two = [frame, frame].concat();
    let first = decode(&two).expect("valid").expect("whole");
    assert_eq!(first.len, frame.len(), "{frame:x?}");
    let rest = two.get(first.len..).expect("in bounds");
    assert_eq!(decode(rest), Ok(Some(first)));
}

#[test]
fn refuses_a_frame_that_is_not_valid() {
    let cases: [(Decoded<'_>, Error, &str); 5] = [
        (
            decode_request(&[1, 0x10, 0, 0, 0, 0x7C, 0xF9]),
            Error::Frame(258),
            "an RTU frame of 258 bytes, over 256",
        ),
        (
            decode_request(&[1, 0x2B]),
            Error::Function(0x2B),
            "function code 43 is not one this connector reads",
        ),
        (
            decode_request(&[1, 0x83]),
            Error::Function(0x83),
            "function code 131 is not one this connector reads",
        ),
        (
            decode_request(&[0x11, 0x03, 0x00, 0x6B, 0x00, 0x03, 0x76, 0x88]),
            Error::Crc {
                want: 0x8776,
                got: 0x8876,
            },
            "a CRC of 0x8876 where the frame gives 0x8776",
        ),
        (
            decode_reply(&READ, &[0x11, 0x83, 0x02, 0xC1, 0x35]),
            Error::Crc {
                want: 0x34C1,
                got: 0x35C1,
            },
            "a CRC of 0x35c1 where the frame gives 0x34c1",
        ),
    ];
    for (got, error, message) in cases {
        assert_eq!(got, Err(error.clone()));
        assert_eq!(error.to_string(), message);
    }
    let mut longest = vec![1, 0x10, 0, 0, 0, 0x7B, 247];
    longest.resize(254, 0);
    let longest = framed(&longest);
    let frame = decode_request(&longest).expect("valid").expect("whole");
    assert_eq!((frame.pdu.len(), frame.len), (253, 256));
}

/// A request that `Request::encode` accepts.
fn request() -> impl Strategy<Value = Request> {
    let tables = prop_oneof![
        Just(Table::Coils),
        Just(Table::DiscreteInputs),
        Just(Table::HoldingRegisters),
        Just(Table::InputRegisters),
    ];
    prop_oneof![
        (tables, 0..=60_000_u16, 1..=125_u16).prop_map(|(table, start, count)| {
            Request::Read {
                table,
                start,
                count,
            }
        }),
        (any::<u16>(), any::<bool>())
            .prop_map(|(address, value)| Request::WriteCoil { address, value }),
        (any::<u16>(), any::<u16>())
            .prop_map(|(address, value)| Request::WriteRegister { address, value }),
        (
            0..=60_000_u16,
            prop::collection::vec(any::<bool>(), 1..=1968)
        )
            .prop_map(|(start, values)| Request::WriteCoils { start, values }),
        (0..=60_000_u16, prop::collection::vec(any::<u16>(), 1..=123))
            .prop_map(|(start, values)| Request::WriteRegisters { start, values }),
    ]
}

proptest! {
    #[test]
    fn steps_as_the_specification_does(bytes in prop::collection::vec(any::<u8>(), 0..300)) {
        prop_assert_eq!(crc(&bytes), bitwise(&bytes));
    }

    #[test]
    fn reads_back_each_request(unit in any::<u8>(), request in request()) {
        let frame = encoded(unit, &request);
        let mut pdu = Vec::new();
        request.encode(&mut pdu).expect("valid");
        prop_assert_eq!(&frame, &framed(&[&[unit], pdu.as_slice()].concat()));
        let read = decode_request(&frame).expect("valid").expect("whole");
        prop_assert_eq!(read, Frame { unit, pdu: &pdu, len: frame.len() });
        prop_assert_eq!(Request::decode(read.pdu), Ok(request));
    }

    /// The CRC catches each one-bit error in a frame of the same length. A flip in
    /// a function or byte count changes the length, and then nothing is certain.
    #[test]
    fn refuses_a_flipped_bit_in_a_request(
        request in request(),
        bit in any::<prop::sample::Index>(),
    ) {
        let mut frame = encoded(1, &request);
        let bit = bit.index(frame.len().saturating_mul(8));
        let byte = frame.get_mut(bit / 8).expect("in bounds");
        *byte ^= 1 << (bit % 8);
        match decode_request(&frame) {
            Ok(Some(read)) => prop_assert_ne!(read.len, frame.len()),
            Ok(None) | Err(Error::Crc { .. } | Error::Function(_) | Error::Frame(_)) => {}
            Err(error) => return Err(TestCaseError::fail(error.to_string())),
        }
    }

    /// With the request known, only the exception flag sets a reply's length, so
    /// the CRC catches a flip of any other bit.
    #[test]
    fn refuses_a_flipped_bit_in_a_reply(
        request in request(),
        fill in any::<u8>(),
        bit in any::<prop::sample::Index>(),
    ) {
        let mut frame = reply(&request, fill);
        let read = decode_reply(&request, &frame).expect("valid").expect("whole");
        prop_assert_eq!(read.len, frame.len());
        let reply = request.decode_reply(read.pdu);
        prop_assert!(!matches!(reply, Ok(Reply::Exception(_)) | Err(_)), "{reply:?}");

        let bit = bit.index(frame.len().saturating_mul(8));
        prop_assume!(bit != 15, "the exception flag");
        let byte = frame.get_mut(bit / 8).expect("in bounds");
        *byte ^= 1 << (bit % 8);
        let got = decode_reply(&request, &frame);
        prop_assert!(matches!(got, Err(Error::Crc { .. })), "{got:?}");
    }
}

/// A framed reply from unit 1 that does `request`, with each register byte
/// `fill`.
fn reply(request: &Request, fill: u8) -> Vec<u8> {
    let mut pdu = Vec::new();
    request.encode(&mut pdu).expect("valid");
    let mut body = vec![1];
    match request {
        Request::Read { table, count, .. } => {
            let count = usize::from(*count);
            let (bytes, fill) = match table {
                Table::Coils | Table::DiscreteInputs => (count.div_ceil(8), 0),
                Table::HoldingRegisters | Table::InputRegisters => {
                    (count.saturating_mul(2), fill)
                }
            };
            body.extend(pdu.first());
            body.push(u8::try_from(bytes).expect("at most 250"));
            body.extend(std::iter::repeat_n(fill, bytes));
        }
        _ => body.extend(pdu.iter().take(5)),
    }
    framed(&body)
}
