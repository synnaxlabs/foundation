//! `connector_modbus` never panics on a Modbus RTU stream. A request frame it
//! reads encodes to the same bytes, and a reply to that request reads as many
//! items as the request asked for.
//!
//! Input: one request frame, then one reply frame.

#![no_main]

use connector_modbus::pdu::{Reply, Request};
use connector_modbus::rtu;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let Ok(Some(frame)) = rtu::decode_request(bytes) else {
        return;
    };
    let Ok(request) = Request::decode(frame.pdu) else {
        return;
    };
    let mut out = Vec::new();
    rtu::encode(frame.unit, &request, &mut out).expect("a decoded request encodes");
    assert_eq!(out, bytes[..frame.len], "the frame changed");

    let Ok(Some(reply)) = rtu::decode_reply(&request, &bytes[frame.len..]) else {
        return;
    };
    let want = match &request {
        Request::Read { count, .. } => usize::from(*count),
        _ => 0,
    };
    match request.decode_reply(reply.pdu) {
        Ok(Reply::Bits(bits)) => {
            assert_eq!((bits.len(), bits.iter().count()), (want, want));
        }
        Ok(Reply::Registers(registers)) => {
            assert_eq!((registers.len(), registers.iter().count()), (want, want));
        }
        _ => {}
    }
});
