//! `connector_modbus` never panics on a Modbus TCP stream. A request it reads
//! encodes to a frame that reads back the same, and a reply to that request reads
//! as many items as the request asked for.
//!
//! Input: one frame, then the PDU of a reply to the request in it.

#![no_main]

use connector_modbus::pdu::{Request, Response};
use connector_modbus::tcp;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let Ok(Some(frame)) = tcp::decode(bytes) else {
        return;
    };
    let Ok(request) = Request::decode(frame.pdu) else {
        return;
    };
    let mut out = Vec::new();
    tcp::encode(frame.header, &request, &mut out).expect("a read request encodes");
    let again = tcp::decode(&out).expect("valid").expect("whole");
    assert_eq!(again.header, frame.header, "the header changed");
    assert_eq!(again.len, out.len(), "the frame length changed");
    assert_eq!(Request::decode(again.pdu), Ok(request.clone()), "the request changed");

    let want = match &request {
        Request::Read { count, .. } => usize::from(*count),
        _ => 0,
    };
    match request.decode_reply(&bytes[frame.len..]) {
        Ok(Response::Bits(bits)) => {
            assert_eq!((bits.len(), bits.iter().count()), (want, want));
        }
        Ok(Response::Registers(registers)) => {
            assert_eq!((registers.len(), registers.iter().count()), (want, want));
        }
        _ => {}
    }
});
