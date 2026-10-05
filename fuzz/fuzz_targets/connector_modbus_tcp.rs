//! `connector_modbus` never panics on a Modbus TCP stream. A request frame it
//! reads encodes to the same bytes, and a reply to that request reads as many
//! items as the request asked for. A device answers every request PDU with a
//! reply that the request reads, or with an exception.
//!
//! Input: one frame, then the PDU of a reply to the request in it.

#![no_main]

use connector_modbus::device::Device;
use connector_modbus::pdu::{Exception, Reply, Request};
use connector_modbus::tcp;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    let Ok(Some(frame)) = tcp::decode(bytes) else {
        return;
    };
    let mut device = Device {
        coils: vec![false; 300],
        discrete_inputs: vec![true; 300],
        holding_registers: vec![1; 300],
        input_registers: vec![2; 300],
    };
    let mut answer = Vec::new();
    device.answer(frame.pdu, &mut answer);
    let Ok(request) = Request::decode(frame.pdu) else {
        assert!(
            matches!(answer[..], [function, 1..=3] if function & 0x80 != 0),
            "{answer:x?} is not an exception"
        );
        return;
    };
    match request.decode_reply(&answer) {
        Ok(Reply::Exception(exception)) => {
            assert_eq!(exception, Exception::IllegalAddress);
        }
        Ok(_) => {}
        Err(error) => panic!("the device's reply does not read: {error}"),
    }

    let mut out = Vec::new();
    tcp::encode(frame.header, &request, &mut out).expect("a decoded request encodes");
    assert_eq!(out, bytes[..frame.len], "the frame changed");

    let want = match &request {
        Request::Read { count, .. } => usize::from(*count),
        _ => 0,
    };
    match request.decode_reply(&bytes[frame.len..]) {
        Ok(Reply::Bits(bits)) => {
            assert_eq!((bits.len(), bits.iter().count()), (want, want));
        }
        Ok(Reply::Registers(registers)) => {
            assert_eq!((registers.len(), registers.iter().count()), (want, want));
        }
        _ => {}
    }
});
