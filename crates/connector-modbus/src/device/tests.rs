use proptest::prelude::*;

use super::*;
use crate::pdu::Reply;

fn bits(bytes: &[u8], count: usize) -> Vec<bool> {
    bytes
        .iter()
        .flat_map(|&byte| (0..8).map(move |bit| byte >> bit & 1 == 1))
        .take(count)
        .collect()
}

/// A device that holds the values of the specification's examples.
fn example() -> Device {
    let mut coils = vec![false; 19];
    coils.extend(bits(&[0xCD, 0x6B, 0x05], 19));
    coils.resize(200, false);
    let mut discrete_inputs = vec![false; 196];
    discrete_inputs.extend(bits(&[0xAC, 0xDB, 0x35], 22));
    let mut holding_registers = vec![0; 110];
    holding_registers.splice(107..110, [0x022B, 0x0000, 0x0064]);
    let mut input_registers = vec![0; 9];
    input_registers[8] = 0x000A;
    Device {
        coils,
        discrete_inputs,
        holding_registers,
        input_registers,
    }
}

fn answer(device: &mut Device, pdu: &[u8]) -> Vec<u8> {
    let mut out = vec![9];
    device.answer(pdu, &mut out);
    assert_eq!(out.first(), Some(&9), "out keeps what it held");
    out.split_off(1)
}

#[test]
fn reads_as_the_specification_gives() {
    let mut device = example();
    let cases: [(&[u8], &[u8]); 4] = [
        (
            &[0x01, 0x00, 0x13, 0x00, 0x13],
            &[0x01, 0x03, 0xCD, 0x6B, 0x05],
        ),
        (
            &[0x02, 0x00, 0xC4, 0x00, 0x16],
            &[0x02, 0x03, 0xAC, 0xDB, 0x35],
        ),
        (
            &[0x03, 0x00, 0x6B, 0x00, 0x03],
            &[0x03, 0x06, 0x02, 0x2B, 0x00, 0x00, 0x00, 0x64],
        ),
        (&[0x04, 0x00, 0x08, 0x00, 0x01], &[0x04, 0x02, 0x00, 0x0A]),
    ];
    for (request, reply) in cases {
        assert_eq!(answer(&mut device, request), reply, "{request:x?}");
    }
    assert_eq!(device, example(), "a read changes nothing");
}

#[test]
fn writes_as_the_specification_gives() {
    let mut device = example();
    let cases: [(&[u8], &[u8]); 4] = [
        (
            &[0x05, 0x00, 0xAC, 0xFF, 0x00],
            &[0x05, 0x00, 0xAC, 0xFF, 0x00],
        ),
        (
            &[0x06, 0x00, 0x01, 0x00, 0x03],
            &[0x06, 0x00, 0x01, 0x00, 0x03],
        ),
        (
            &[0x0F, 0x00, 0x13, 0x00, 0x0A, 0x02, 0xCD, 0x01],
            &[0x0F, 0x00, 0x13, 0x00, 0x0A],
        ),
        (
            &[0x10, 0x00, 0x01, 0x00, 0x02, 0x04, 0x00, 0x0A, 0x01, 0x02],
            &[0x10, 0x00, 0x01, 0x00, 0x02],
        ),
    ];
    for (request, reply) in cases {
        assert_eq!(answer(&mut device, request), reply, "{request:x?}");
    }
    let mut want = example();
    want.coils[172] = true;
    want.coils.splice(19..29, bits(&[0xCD, 0x01], 10));
    want.holding_registers.splice(1..3, [0x000A, 0x0102]);
    assert_eq!(device, want);
}

#[test]
fn refuses_a_request_that_is_not_valid() {
    let cases: [(&[u8], [u8; 2]); 15] = [
        (&[0x2B, 0x0E], [0xAB, 1]),
        (&[0x85], [0x85, 1]),
        (&[0x03, 0x00, 0x00, 0x00], [0x83, 3]),
        (&[0x01, 0x00, 0x00, 0x00, 0x00], [0x81, 3]),
        (&[0x05, 0x00, 0x01, 0x00, 0x01], [0x85, 3]),
        (&[0x10, 0x00, 0x00, 0x00, 0x02, 0x03, 0, 0, 0], [0x90, 3]),
        (&[0x10, 0xFF, 0xFF, 0x00, 0x02, 0x03, 0, 0, 0], [0x90, 3]),
        (&[0x10, 0xFF, 0xFF, 0x00, 0x02, 0x04, 0, 0], [0x90, 3]),
        (&[0x03, 0xFF, 0xFF, 0x00, 0x02], [0x83, 2]),
        (&[0x03, 0x00, 0x6C, 0x00, 0x03], [0x83, 2]),
        (&[0x05, 0x00, 0xC8, 0xFF, 0x00], [0x85, 2]),
        (&[0x06, 0x00, 0x6E, 0x00, 0x01], [0x86, 2]),
        (&[0x10, 0x00, 0x6D, 0x00, 0x02, 0x04, 0, 1, 0, 1], [0x90, 2]),
        // Stricter than the specification, which does not check the unused bits.
        (&[0x0F, 0x00, 0x00, 0x00, 0x01, 0x01, 0xFF], [0x8F, 3]),
        (&[0x0F, 0xFF, 0xFF, 0x00, 0x02, 0x01, 0xFF], [0x8F, 3]),
    ];
    let mut device = example();
    for (request, reply) in cases {
        assert_eq!(answer(&mut device, request), reply, "{request:x?}");
        assert_eq!(device, example(), "{request:x?} changes nothing");
    }
    assert_eq!(answer(&mut device, &[]), [], "an empty PDU gets no reply");
}

const BITS: u16 = 2500;
const REGISTERS: u16 = 400;

/// A request that `Request::encode` accepts, near the device's tables.
fn request() -> impl Strategy<Value = Request> {
    let bits = prop_oneof![Just(Table::Coils), Just(Table::DiscreteInputs)];
    let registers =
        prop_oneof![Just(Table::HoldingRegisters), Just(Table::InputRegisters)];
    prop_oneof![
        (bits, 0..=BITS, 1..=2000_u16),
        (registers, 0..=REGISTERS, 1..=125_u16),
    ]
    .prop_map(|(table, start, count)| Request::Read {
        table,
        start,
        count,
    })
    .boxed()
    .prop_union(
        prop_oneof![
            (0..=BITS, any::<bool>())
                .prop_map(|(address, value)| Request::WriteCoil { address, value }),
            (0..=REGISTERS, any::<u16>()).prop_map(|(address, value)| {
                Request::WriteRegister { address, value }
            }),
            (0..=BITS, prop::collection::vec(any::<bool>(), 1..=1968))
                .prop_map(|(start, values)| Request::WriteCoils { start, values }),
            (0..=REGISTERS, prop::collection::vec(any::<u16>(), 1..=123))
                .prop_map(|(start, values)| Request::WriteRegisters { start, values }),
        ]
        .boxed(),
    )
}

fn device() -> impl Strategy<Value = Device> {
    let bits = || prop::collection::vec(any::<bool>(), 0..=usize::from(BITS));
    let registers = || prop::collection::vec(any::<u16>(), 0..=usize::from(REGISTERS));
    (bits(), bits(), registers(), registers()).prop_map(
        |(coils, discrete_inputs, holding_registers, input_registers)| Device {
            coils,
            discrete_inputs,
            holding_registers,
            input_registers,
        },
    )
}

/// Where `address` sits in the `count` addresses from `start`, if it does.
fn offset(address: usize, start: u16, count: usize) -> Option<usize> {
    address
        .checked_sub(usize::from(start))
        .filter(|&offset| offset < count)
}

/// The values at the `count` addresses from `start`, or `None` when one is not in
/// `table`.
fn get<T: Copy>(table: &[T], start: u16, count: usize) -> Option<Vec<T>> {
    let values: Vec<T> = table
        .iter()
        .enumerate()
        .filter(|&(address, _)| offset(address, start, count).is_some())
        .map(|(_, &value)| value)
        .collect();
    (values.len() == count).then_some(values)
}

/// Sets each address from `start` to its value, or gives `None` when one is not in
/// `table`.
fn set<T: Copy>(table: &mut [T], start: u16, values: &[T]) -> Option<()> {
    get(table, start, values.len())?;
    for (address, slot) in table.iter_mut().enumerate() {
        if let Some(offset) = offset(address, start, values.len()) {
            *slot = *values.get(offset)?;
        }
    }
    Some(())
}

/// The device after `request`, and the values it reads, or `None` when the
/// request's range is not in the device.
fn model(device: &Device, request: &Request) -> Option<(Device, Vec<u16>)> {
    let mut after = device.clone();
    let read = match request {
        Request::Read {
            table,
            start,
            count,
        } => {
            let count = usize::from(*count);
            let bits = |table: &[bool]| -> Option<Vec<u16>> {
                Some(
                    get(table, *start, count)?
                        .into_iter()
                        .map(u16::from)
                        .collect(),
                )
            };
            match table {
                Table::Coils => bits(&device.coils)?,
                Table::DiscreteInputs => bits(&device.discrete_inputs)?,
                Table::HoldingRegisters => {
                    get(&device.holding_registers, *start, count)?
                }
                Table::InputRegisters => get(&device.input_registers, *start, count)?,
            }
        }
        Request::WriteCoil { address, value } => {
            set(&mut after.coils, *address, &[*value])?;
            Vec::new()
        }
        Request::WriteRegister { address, value } => {
            set(&mut after.holding_registers, *address, &[*value])?;
            Vec::new()
        }
        Request::WriteCoils { start, values } => {
            set(&mut after.coils, *start, values)?;
            Vec::new()
        }
        Request::WriteRegisters { start, values } => {
            set(&mut after.holding_registers, *start, values)?;
            Vec::new()
        }
    };
    Some((after, read))
}

proptest! {
    #[test]
    fn answers_as_the_model_does(before in device(), request in request()) {
        let mut pdu = Vec::new();
        request.encode(&mut pdu).expect("valid");
        let mut device = before.clone();
        let reply = answer(&mut device, &pdu);
        let read: Vec<u16> = match request.decode_reply(&reply) {
            Ok(Reply::Bits(bits)) => bits.iter().map(u16::from).collect(),
            Ok(Reply::Registers(registers)) => registers.iter().collect(),
            Ok(Reply::Written) => Vec::new(),
            Ok(Reply::Exception(exception)) => {
                prop_assert_eq!(exception, Exception::IllegalAddress);
                prop_assert_eq!(model(&before, &request), None);
                prop_assert_eq!(device, before, "an exception changes nothing");
                return Ok(());
            }
            Err(error) => return Err(TestCaseError::fail(error.to_string())),
        };
        prop_assert_eq!(Some((device, read)), model(&before, &request));
    }
}
