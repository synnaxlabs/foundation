//! The Modbus PDU: requests, and the replies to them read in place.

use crate::Error;

const READ_BITS: u16 = 2000;
const READ_REGISTERS: u16 = 125;
const WRITE_BITS: u16 = 1968;
const WRITE_REGISTERS: u16 = 123;
const ON: u16 = 0xFF00;
const EXCEPTION: u8 = 0x80;

/// One of the four Modbus data tables.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Table {
    /// Read-write bits.
    Coils,
    /// Read-only bits.
    DiscreteInputs,
    /// Read-write 16-bit registers.
    HoldingRegisters,
    /// Read-only 16-bit registers.
    InputRegisters,
}

impl Table {
    fn function(self) -> u8 {
        match self {
            Self::Coils => 1,
            Self::DiscreteInputs => 2,
            Self::HoldingRegisters => 3,
            Self::InputRegisters => 4,
        }
    }

    fn bits(self) -> bool {
        matches!(self, Self::Coils | Self::DiscreteInputs)
    }
}

/// A Modbus request. Counts and ranges are checked when it is encoded or decoded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    /// Reads `count` items of `table` from `start` (functions 1 to 4).
    Read {
        /// The table to read.
        table: Table,
        /// The first address.
        start: u16,
        /// The number of items: 1 to 2000 bits or 1 to 125 registers.
        count: u16,
    },
    /// Writes one coil (function 5).
    WriteCoil {
        /// The coil's address.
        address: u16,
        /// The value.
        value: bool,
    },
    /// Writes one holding register (function 6).
    WriteRegister {
        /// The register's address.
        address: u16,
        /// The value.
        value: u16,
    },
    /// Writes 1 to 1968 coils from `start` (function 15).
    WriteCoils {
        /// The first address.
        start: u16,
        /// The values, in address order.
        values: Vec<bool>,
    },
    /// Writes 1 to 123 holding registers from `start` (function 16).
    WriteRegisters {
        /// The first address.
        start: u16,
        /// The values, in address order.
        values: Vec<u16>,
    },
}

impl Request {
    /// Appends the request's PDU to `out`.
    ///
    /// # Errors
    ///
    /// [`Error::Count`] or [`Error::Range`]. `out` is unchanged then.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        self.size()?;
        out.push(self.function());
        match self {
            Self::Read { start, count, .. } => put(out, [*start, *count]),
            Self::WriteCoil { address, value } => put(out, [*address, coil(*value)]),
            Self::WriteRegister { address, value } => put(out, [*address, *value]),
            Self::WriteCoils { start, values } => {
                put(out, [*start, len(values)]);
                out.push(byte_count(values.len().div_ceil(8)));
                for byte in values.chunks(8) {
                    out.push(pack(byte));
                }
            }
            Self::WriteRegisters { start, values } => {
                put(out, [*start, len(values)]);
                out.push(byte_count(values.len().saturating_mul(2)));
                for value in values {
                    out.extend_from_slice(&value.to_be_bytes());
                }
            }
        }
        Ok(())
    }

    /// Reads a request PDU, as a device does.
    ///
    /// # Errors
    ///
    /// [`Error::Size`], [`Error::Function`], [`Error::Count`], [`Error::Range`],
    /// [`Error::Bytes`], or [`Error::Coil`].
    pub fn decode(pdu: &[u8]) -> Result<Self, Error> {
        let Some(&function) = pdu.first() else {
            return Err(Error::Size { want: 1, got: 0 });
        };
        let request = match function {
            1..=4 => {
                let [start, count] = fields(pdu)?;
                let table = match function {
                    1 => Table::Coils,
                    2 => Table::DiscreteInputs,
                    3 => Table::HoldingRegisters,
                    _ => Table::InputRegisters,
                };
                Self::Read {
                    table,
                    start,
                    count,
                }
            }
            5 => {
                let [address, value] = fields(pdu)?;
                let value = match value {
                    ON => true,
                    0 => false,
                    other => return Err(Error::Coil(other)),
                };
                Self::WriteCoil { address, value }
            }
            6 => {
                let [address, value] = fields(pdu)?;
                Self::WriteRegister { address, value }
            }
            15 | 16 => Self::decode_writes(function, pdu)?,
            other => return Err(Error::Function(other)),
        };
        request.size()?;
        Ok(request)
    }

    /// Reads a PDU of function 15 or 16.
    fn decode_writes(function: u8, pdu: &[u8]) -> Result<Self, Error> {
        let Some(([_, s0, s1, c0, c1, bytes], data)) = pdu.split_first_chunk::<6>()
        else {
            return Err(Error::Size {
                want: 6,
                got: pdu.len(),
            });
        };
        let start = u16::from_be_bytes([*s0, *s1]);
        let count = u16::from_be_bytes([*c0, *c1]);
        let (max, want) = if function == 15 {
            (WRITE_BITS, usize::from(count).div_ceil(8))
        } else {
            (WRITE_REGISTERS, usize::from(count).saturating_mul(2))
        };
        check(start, count, max)?;
        if usize::from(*bytes) != want {
            return Err(Error::Bytes {
                want,
                got: usize::from(*bytes),
            });
        }
        if data.len() != want {
            return Err(Error::Size {
                want: want.saturating_add(6),
                got: pdu.len(),
            });
        }
        Ok(if function == 15 {
            let bits = Bits { bytes: data, count };
            Self::WriteCoils {
                start,
                values: bits.iter().collect(),
            }
        } else {
            Self::WriteRegisters {
                start,
                values: Registers(data).iter().collect(),
            }
        })
    }

    /// Reads the device's reply PDU to this request, in place.
    ///
    /// # Errors
    ///
    /// [`Error::Size`], [`Error::Answer`], [`Error::Bytes`], or [`Error::Echo`]; and
    /// [`Error::Count`] or [`Error::Range`] when this request is not valid.
    pub fn decode_reply<'a>(&self, pdu: &'a [u8]) -> Result<Response<'a>, Error> {
        self.size()?;
        let want = self.function();
        let Some((&got, body)) = pdu.split_first() else {
            return Err(Error::Size { want: 2, got: 0 });
        };
        if got == want | EXCEPTION {
            let [code] = body else {
                return Err(Error::Size {
                    want: 2,
                    got: pdu.len(),
                });
            };
            return Ok(Response::Exception(Exception::from(*code)));
        }
        if got != want {
            return Err(Error::Answer { want, got });
        }
        match self {
            Self::Read { table, count, .. } => {
                let Some((&bytes, data)) = body.split_first() else {
                    return Err(Error::Size {
                        want: 2,
                        got: pdu.len(),
                    });
                };
                let want = if table.bits() {
                    usize::from(*count).div_ceil(8)
                } else {
                    usize::from(*count).saturating_mul(2)
                };
                if usize::from(bytes) != want {
                    return Err(Error::Bytes {
                        want,
                        got: usize::from(bytes),
                    });
                }
                if data.len() != want {
                    return Err(Error::Size {
                        want: want.saturating_add(2),
                        got: pdu.len(),
                    });
                }
                Ok(if table.bits() {
                    Response::Bits(Bits {
                        bytes: data,
                        count: *count,
                    })
                } else {
                    Response::Registers(Registers(data))
                })
            }
            Self::WriteCoil { address, value } => echo(pdu, [*address, coil(*value)]),
            Self::WriteRegister { address, value } => echo(pdu, [*address, *value]),
            Self::WriteCoils { start, values } => echo(pdu, [*start, len(values)]),
            Self::WriteRegisters { start, values } => echo(pdu, [*start, len(values)]),
        }
    }

    fn function(&self) -> u8 {
        match self {
            Self::Read { table, .. } => table.function(),
            Self::WriteCoil { .. } => 5,
            Self::WriteRegister { .. } => 6,
            Self::WriteCoils { .. } => 15,
            Self::WriteRegisters { .. } => 16,
        }
    }

    /// The length of the request's PDU, after its counts and range are checked.
    pub(crate) fn size(&self) -> Result<usize, Error> {
        match self {
            Self::Read {
                table,
                start,
                count,
            } => {
                let max = if table.bits() {
                    READ_BITS
                } else {
                    READ_REGISTERS
                };
                check(*start, *count, max)?;
                Ok(5)
            }
            Self::WriteCoil { .. } | Self::WriteRegister { .. } => Ok(5),
            Self::WriteCoils { start, values } => {
                check(*start, len(values), WRITE_BITS)?;
                Ok(values.len().div_ceil(8).saturating_add(6))
            }
            Self::WriteRegisters { start, values } => {
                check(*start, len(values), WRITE_REGISTERS)?;
                Ok(values.len().saturating_mul(2).saturating_add(6))
            }
        }
    }
}

/// A device's reply to a [`Request`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response<'a> {
    /// The coils or discrete inputs read.
    Bits(Bits<'a>),
    /// The registers read.
    Registers(Registers<'a>),
    /// The device did the write.
    Written,
    /// The device refused the request.
    Exception(Exception),
}

/// The bits of a read reply, read in place, in address order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bits<'a> {
    bytes: &'a [u8],
    count: u16,
}

impl<'a> Bits<'a> {
    /// The number of bits.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.count)
    }

    /// Whether the reply holds no bits. A valid reply never does.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Each bit, in address order.
    pub fn iter(&self) -> impl Iterator<Item = bool> + 'a {
        self.bytes
            .iter()
            .flat_map(|&byte| (0..8).map(move |bit| byte >> bit & 1 == 1))
            .take(self.len())
    }
}

/// The registers of a read reply, read in place, in address order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Registers<'a>(&'a [u8]);

impl<'a> Registers<'a> {
    /// The number of registers.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.as_chunks::<2>().0.len()
    }

    /// Whether the reply holds no registers. A valid reply never does.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Each register, in address order.
    pub fn iter(&self) -> impl Iterator<Item = u16> + 'a {
        self.0
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&pair| u16::from_be_bytes(pair))
    }
}

/// The exception codes of the Modbus specification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exception {
    /// 1: the device does not have the function.
    IllegalFunction,
    /// 2: an address is not in the device.
    IllegalAddress,
    /// 3: a value in the request is not valid for the device.
    IllegalValue,
    /// 4: the device failed while it did the request.
    DeviceFailure,
    /// 5: the device accepted a long request and is still doing it.
    Acknowledge,
    /// 6: the device is busy with a long request.
    Busy,
    /// 10: a gateway has no path to the target.
    GatewayPath,
    /// 11: the target behind a gateway did not reply.
    GatewayTarget,
    /// Any other code.
    Other(u8),
}

impl From<u8> for Exception {
    fn from(code: u8) -> Self {
        match code {
            1 => Self::IllegalFunction,
            2 => Self::IllegalAddress,
            3 => Self::IllegalValue,
            4 => Self::DeviceFailure,
            5 => Self::Acknowledge,
            6 => Self::Busy,
            10 => Self::GatewayPath,
            11 => Self::GatewayTarget,
            other => Self::Other(other),
        }
    }
}

fn check(start: u16, count: u16, max: u16) -> Result<(), Error> {
    if count == 0 || count > max {
        return Err(Error::Count { count, max });
    }
    if u32::from(start).saturating_add(u32::from(count)) > 0x1_0000 {
        return Err(Error::Range { start, count });
    }
    Ok(())
}

/// The count of `values`, or `u16::MAX` when it does not fit, which no check passes.
fn len<T>(values: &[T]) -> u16 {
    u16::try_from(values.len()).unwrap_or(u16::MAX)
}

fn byte_count(n: usize) -> u8 {
    u8::try_from(n).expect("invariant: a checked count fits a byte count")
}

fn coil(value: bool) -> u16 {
    if value { ON } else { 0 }
}

fn pack(bits: &[bool]) -> u8 {
    bits.iter()
        .rev()
        .fold(0, |byte, &bit| byte << 1 | u8::from(bit))
}

fn put(out: &mut Vec<u8>, fields: [u16; 2]) {
    for field in fields {
        out.extend_from_slice(&field.to_be_bytes());
    }
}

/// The two fields of a 5-byte PDU.
fn fields(pdu: &[u8]) -> Result<[u16; 2], Error> {
    let [_, a0, a1, b0, b1] = pdu else {
        return Err(Error::Size {
            want: 5,
            got: pdu.len(),
        });
    };
    Ok([
        u16::from_be_bytes([*a0, *a1]),
        u16::from_be_bytes([*b0, *b1]),
    ])
}

fn echo(pdu: &[u8], want: [u16; 2]) -> Result<Response<'_>, Error> {
    let got = fields(pdu)?;
    if got != want {
        return Err(Error::Echo { want, got });
    }
    Ok(Response::Written)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn encoded(request: &Request) -> Vec<u8> {
        let mut out = Vec::new();
        request.encode(&mut out).expect("a valid request");
        out
    }

    fn read(table: Table, start: u16, count: u16) -> Request {
        Request::Read {
            table,
            start,
            count,
        }
    }

    /// The examples of the Modbus Application Protocol specification v1.1b3.
    #[test]
    fn matches_the_read_examples_of_the_specification() {
        let coils = read(Table::Coils, 19, 19);
        assert_eq!(encoded(&coils), [0x01, 0x00, 0x13, 0x00, 0x13]);
        let Ok(Response::Bits(bits)) =
            coils.decode_reply(&[0x01, 0x03, 0xCD, 0x6B, 0x05])
        else {
            panic!("bits");
        };
        let want = "1011001111010110101";
        let got: String = bits.iter().map(|b| if b { '1' } else { '0' }).collect();
        assert_eq!((bits.len(), got.as_str()), (19, want));

        let inputs = read(Table::DiscreteInputs, 196, 22);
        assert_eq!(encoded(&inputs), [0x02, 0x00, 0xC4, 0x00, 0x16]);
        let Ok(Response::Bits(bits)) =
            inputs.decode_reply(&[0x02, 0x03, 0xAC, 0xDB, 0x35])
        else {
            panic!("bits");
        };
        assert_eq!(bits.iter().filter(|&b| b).count(), 14);

        let holding = read(Table::HoldingRegisters, 107, 3);
        assert_eq!(encoded(&holding), [0x03, 0x00, 0x6B, 0x00, 0x03]);
        let reply = [0x03, 0x06, 0x02, 0x2B, 0x00, 0x00, 0x00, 0x64];
        let Ok(Response::Registers(registers)) = holding.decode_reply(&reply) else {
            panic!("registers");
        };
        let values: Vec<u16> = registers.iter().collect();
        assert_eq!((registers.len(), values), (3, vec![0x022B, 0, 0x64]));

        let input = read(Table::InputRegisters, 8, 1);
        assert_eq!(encoded(&input), [0x04, 0x00, 0x08, 0x00, 0x01]);
        let Ok(Response::Registers(registers)) =
            input.decode_reply(&[0x04, 0x02, 0, 10])
        else {
            panic!("registers");
        };
        assert_eq!(registers.iter().collect::<Vec<_>>(), [10]);
    }

    #[test]
    fn matches_the_write_examples_of_the_specification() {
        let writes = [
            (
                Request::WriteCoil {
                    address: 172,
                    value: true,
                },
                vec![0x05, 0x00, 0xAC, 0xFF, 0x00],
                vec![0x05, 0x00, 0xAC, 0xFF, 0x00],
            ),
            (
                Request::WriteRegister {
                    address: 1,
                    value: 3,
                },
                vec![0x06, 0x00, 0x01, 0x00, 0x03],
                vec![0x06, 0x00, 0x01, 0x00, 0x03],
            ),
            (
                Request::WriteCoils {
                    start: 19,
                    values: [1, 0, 1, 1, 0, 0, 1, 1, 1, 0].map(|b| b == 1).to_vec(),
                },
                vec![0x0F, 0x00, 0x13, 0x00, 0x0A, 0x02, 0xCD, 0x01],
                vec![0x0F, 0x00, 0x13, 0x00, 0x0A],
            ),
            (
                Request::WriteRegisters {
                    start: 1,
                    values: vec![0x000A, 0x0102],
                },
                vec![0x10, 0x00, 0x01, 0x00, 0x02, 0x04, 0x00, 0x0A, 0x01, 0x02],
                vec![0x10, 0x00, 0x01, 0x00, 0x02],
            ),
        ];
        for (request, pdu, reply) in writes {
            assert_eq!(encoded(&request), pdu, "{request:?}");
            assert_eq!(Request::decode(&pdu), Ok(request.clone()));
            assert_eq!(request.decode_reply(&reply), Ok(Response::Written));
        }
    }

    #[test]
    fn reads_an_exception_reply() {
        let request = read(Table::HoldingRegisters, 0, 1);
        let codes = [1, 2, 3, 4, 5, 6, 10, 11, 7];
        let want = [
            Exception::IllegalFunction,
            Exception::IllegalAddress,
            Exception::IllegalValue,
            Exception::DeviceFailure,
            Exception::Acknowledge,
            Exception::Busy,
            Exception::GatewayPath,
            Exception::GatewayTarget,
            Exception::Other(7),
        ];
        for (code, want) in codes.into_iter().zip(want) {
            assert_eq!(
                request.decode_reply(&[0x83, code]),
                Ok(Response::Exception(want))
            );
        }
        assert_eq!(
            request.decode_reply(&[0x83]),
            Err(Error::Size { want: 2, got: 1 })
        );
    }

    #[test]
    fn refuses_a_count_or_range_out_of_bounds() {
        let cases = [
            (
                read(Table::Coils, 0, 0),
                Error::Count {
                    count: 0,
                    max: 2000,
                },
            ),
            (
                read(Table::DiscreteInputs, 0, 2001),
                Error::Count {
                    count: 2001,
                    max: 2000,
                },
            ),
            (
                read(Table::InputRegisters, 0, 126),
                Error::Count {
                    count: 126,
                    max: 125,
                },
            ),
            (
                read(Table::HoldingRegisters, 65535, 2),
                Error::Range {
                    start: 65535,
                    count: 2,
                },
            ),
            (
                Request::WriteCoils {
                    start: 0,
                    values: vec![true; 1969],
                },
                Error::Count {
                    count: 1969,
                    max: 1968,
                },
            ),
            (
                Request::WriteRegisters {
                    start: 0,
                    values: vec![],
                },
                Error::Count { count: 0, max: 123 },
            ),
            (
                Request::WriteRegisters {
                    start: 0,
                    values: vec![0; 70_000],
                },
                Error::Count {
                    count: u16::MAX,
                    max: 123,
                },
            ),
        ];
        for (request, error) in cases {
            let mut out = vec![9];
            assert_eq!(request.encode(&mut out), Err(error.clone()));
            assert_eq!(out, [9], "out is unchanged");
            assert_eq!(request.decode_reply(&[]), Err(error));
        }
    }

    #[test]
    fn accepts_a_range_that_ends_at_the_last_address() {
        assert_eq!(
            read(Table::HoldingRegisters, 65535, 1).encode(&mut Vec::new()),
            Ok(()),
            "the last address"
        );
        assert_eq!(
            Error::Range {
                start: 65535,
                count: 2
            }
            .to_string(),
            "2 items from address 65535 run past address 65535"
        );
        assert_eq!(
            Error::Count { count: 0, max: 125 }.to_string(),
            "a count of 0, outside 1 to 125"
        );
    }

    #[test]
    fn refuses_a_request_pdu_that_is_not_valid() {
        let cases: [(&[u8], Error, &str); 9] = [
            (
                &[],
                Error::Size { want: 1, got: 0 },
                "a PDU of 0 bytes where its function gives 1",
            ),
            (
                &[0x03, 0x00, 0x00, 0x00],
                Error::Size { want: 5, got: 4 },
                "a PDU of 4 bytes where its function gives 5",
            ),
            (
                &[0x06, 0x00, 0x00, 0x00, 0x01, 0x00],
                Error::Size { want: 5, got: 6 },
                "a PDU of 6 bytes where its function gives 5",
            ),
            (
                &[0x2B, 0x0E],
                Error::Function(0x2B),
                "function code 43 is not one this connector reads",
            ),
            (
                &[0x05, 0x00, 0x01, 0x00, 0x01],
                Error::Coil(1),
                "a coil value of 0x0001, not 0x0000 or 0xff00",
            ),
            (
                &[0x10, 0x00, 0x00, 0x00, 0x02, 0x03, 0, 0, 0],
                Error::Bytes { want: 4, got: 3 },
                "a byte count of 3 where the item count gives 4",
            ),
            (
                &[0x10, 0x00, 0x00, 0x00, 0x01, 0x02, 0],
                Error::Size { want: 8, got: 7 },
                "a PDU of 7 bytes where its function gives 8",
            ),
            (
                &[0x0F, 0x00, 0x00, 0x00, 0x00, 0x00],
                Error::Count {
                    count: 0,
                    max: 1968,
                },
                "a count of 0, outside 1 to 1968",
            ),
            (
                &[0x0F, 0x00, 0x00, 0x00],
                Error::Size { want: 6, got: 4 },
                "a PDU of 4 bytes where its function gives 6",
            ),
        ];
        for (pdu, error, message) in cases {
            assert_eq!(Request::decode(pdu), Err(error.clone()), "{pdu:x?}");
            assert_eq!(error.to_string(), message);
        }
    }

    #[test]
    fn refuses_a_reply_that_does_not_answer_the_request() {
        let coils = read(Table::Coils, 0, 9);
        let registers = read(Table::InputRegisters, 0, 2);
        let write = Request::WriteRegister {
            address: 7,
            value: 1,
        };
        let cases: [(&Request, &[u8], Error, &str); 7] = [
            (
                &coils,
                &[0x02, 0x02, 0, 0],
                Error::Answer { want: 1, got: 2 },
                "a reply of function 2 to a request of function 1",
            ),
            (
                &coils,
                &[0x01, 0x01, 0],
                Error::Bytes { want: 2, got: 1 },
                "a byte count of 1 where the item count gives 2",
            ),
            (
                &coils,
                &[0x01, 0x02, 0],
                Error::Size { want: 4, got: 3 },
                "a PDU of 3 bytes where its function gives 4",
            ),
            (
                &registers,
                &[],
                Error::Size { want: 2, got: 0 },
                "a PDU of 0 bytes where its function gives 2",
            ),
            (
                &registers,
                &[0x04],
                Error::Size { want: 2, got: 1 },
                "a PDU of 1 bytes where its function gives 2",
            ),
            (
                &write,
                &[0x06, 0x00, 0x07, 0x00, 0x02],
                Error::Echo {
                    want: [7, 1],
                    got: [7, 2],
                },
                "a write reply that echoes [7, 2], not [7, 1]",
            ),
            (
                &write,
                &[0x86, 0x02, 0x00],
                Error::Size { want: 2, got: 3 },
                "a PDU of 3 bytes where its function gives 2",
            ),
        ];
        for (request, pdu, error, message) in cases {
            assert_eq!(request.decode_reply(pdu), Err(error.clone()), "{pdu:x?}");
            assert_eq!(error.to_string(), message);
        }
    }

    fn request() -> impl Strategy<Value = Request> {
        let table = prop_oneof![
            Just(Table::Coils),
            Just(Table::DiscreteInputs),
            Just(Table::HoldingRegisters),
            Just(Table::InputRegisters),
        ];
        prop_oneof![
            (table, 0..=u16::MAX, 1..=125_u16).prop_map(|(table, start, count)| {
                read(
                    table,
                    start.min(u16::MAX.saturating_sub(count).saturating_add(1)),
                    count,
                )
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
        fn decodes_what_it_encodes(request in request()) {
            let pdu = encoded(&request);
            prop_assert_eq!(Some(pdu.len()), request.size().ok());
            prop_assert_eq!(Request::decode(&pdu), Ok(request));
        }

        #[test]
        fn decodes_any_bytes_without_a_panic(pdu in prop::collection::vec(any::<u8>(), 0..300)) {
            if let Ok(request) = Request::decode(&pdu) {
                prop_assert_eq!(Request::decode(&encoded(&request)), Ok(request));
            }
        }
    }
}
