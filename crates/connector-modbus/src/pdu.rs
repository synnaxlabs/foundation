//! The Modbus PDU: requests, and the replies to them read in place.

use crate::Error;

const READ_COILS: u8 = 1;
const READ_DISCRETE_INPUTS: u8 = 2;
const READ_HOLDING_REGISTERS: u8 = 3;
const READ_INPUT_REGISTERS: u8 = 4;
const WRITE_COIL: u8 = 5;
const WRITE_REGISTER: u8 = 6;
const WRITE_COILS: u8 = 15;
const WRITE_REGISTERS: u8 = 16;
const EXCEPTION: u8 = 0x80;

const MAX_READ_BITS: u16 = 2000;
const MAX_READ_REGISTERS: u16 = 125;
const MAX_WRITE_BITS: u16 = 1968;
const MAX_WRITE_REGISTERS: u16 = 123;
const ON: u16 = 0xFF00;

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
            Self::Coils => READ_COILS,
            Self::DiscreteInputs => READ_DISCRETE_INPUTS,
            Self::HoldingRegisters => READ_HOLDING_REGISTERS,
            Self::InputRegisters => READ_INPUT_REGISTERS,
        }
    }

    fn of(function: u8) -> Option<Self> {
        match function {
            READ_COILS => Some(Self::Coils),
            READ_DISCRETE_INPUTS => Some(Self::DiscreteInputs),
            READ_HOLDING_REGISTERS => Some(Self::HoldingRegisters),
            READ_INPUT_REGISTERS => Some(Self::InputRegisters),
            _ => None,
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
        self.write_to(out);
        Ok(())
    }

    /// Reads a request PDU, as a device does. A PDU it accepts encodes to the same
    /// bytes.
    ///
    /// # Errors
    ///
    /// [`Error::Size`], [`Error::Function`], [`Error::Count`], [`Error::Range`],
    /// [`Error::ByteCount`], [`Error::Padding`], or [`Error::Coil`].
    pub fn decode(pdu: &[u8]) -> Result<Self, Error> {
        let Some(&function) = pdu.first() else {
            return Err(Error::Size { want: 1, got: 0 });
        };
        if let Some(table) = Table::of(function) {
            let [start, count] = fields(pdu)?;
            let request = Self::Read {
                table,
                start,
                count,
            };
            request.size()?;
            return Ok(request);
        }
        match function {
            WRITE_COIL => {
                let [address, value] = fields(pdu)?;
                let value = match value {
                    ON => true,
                    0 => false,
                    other => return Err(Error::Coil(other)),
                };
                Ok(Self::WriteCoil { address, value })
            }
            WRITE_REGISTER => {
                let [address, value] = fields(pdu)?;
                Ok(Self::WriteRegister { address, value })
            }
            WRITE_COILS | WRITE_REGISTERS => Self::decode_writes(function, pdu),
            other => Err(Error::Function(other)),
        }
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
        let coils = function == WRITE_COILS;
        let max = if coils {
            MAX_WRITE_BITS
        } else {
            MAX_WRITE_REGISTERS
        };
        let want = data_bytes(coils, usize::from(count));
        if usize::from(*bytes) != want {
            return Err(Error::ByteCount {
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
        let request = if coils {
            Self::WriteCoils {
                start,
                values: Bits::new(data, count)?.iter().collect(),
            }
        } else {
            Self::WriteRegisters {
                start,
                values: Registers(data).iter().collect(),
            }
        };
        // The specification checks every value before the range.
        check(start, usize::from(count), max)?;
        Ok(request)
    }

    /// Reads the device's reply PDU to this request, in place. The request is one
    /// that [`Request::encode`] accepts; for another, the reply never matches.
    ///
    /// # Errors
    ///
    /// [`Error::Size`], [`Error::Answer`], [`Error::ByteCount`], [`Error::Padding`],
    /// or [`Error::Echo`].
    pub fn decode_reply<'a>(&self, pdu: &'a [u8]) -> Result<Reply<'a>, Error> {
        let size = self.reply_size();
        let want = self.function();
        let Some((&got, body)) = pdu.split_first() else {
            return Err(Error::Size { want: size, got: 0 });
        };
        if got == want | EXCEPTION {
            let [code] = body else {
                return Err(Error::Size {
                    want: 2,
                    got: pdu.len(),
                });
            };
            return Ok(Reply::Exception(Exception::from(*code)));
        }
        if got != want {
            return Err(Error::Answer { want, got });
        }
        match self {
            Self::Read { table, count, .. } => {
                let Some((&bytes, data)) = body.split_first() else {
                    return Err(Error::Size {
                        want: size,
                        got: pdu.len(),
                    });
                };
                let want = data_bytes(table.bits(), usize::from(*count));
                if usize::from(bytes) != want {
                    return Err(Error::ByteCount {
                        want,
                        got: usize::from(bytes),
                    });
                }
                if data.len() != want {
                    return Err(Error::Size {
                        want: size,
                        got: pdu.len(),
                    });
                }
                Ok(if table.bits() {
                    Reply::Bits(Bits::new(data, *count)?)
                } else {
                    Reply::Registers(Registers(data))
                })
            }
            _ => echo(pdu, self.head()),
        }
    }

    fn function(&self) -> u8 {
        match self {
            Self::Read { table, .. } => table.function(),
            Self::WriteCoil { .. } => WRITE_COIL,
            Self::WriteRegister { .. } => WRITE_REGISTER,
            Self::WriteCoils { .. } => WRITE_COILS,
            Self::WriteRegisters { .. } => WRITE_REGISTERS,
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
                    MAX_READ_BITS
                } else {
                    MAX_READ_REGISTERS
                };
                check(*start, usize::from(*count), max)?;
                Ok(5)
            }
            Self::WriteCoil { .. } | Self::WriteRegister { .. } => Ok(5),
            Self::WriteCoils { start, values } => {
                check(*start, values.len(), MAX_WRITE_BITS)?;
                Ok(data_bytes(true, values.len()).saturating_add(6))
            }
            Self::WriteRegisters { start, values } => {
                check(*start, values.len(), MAX_WRITE_REGISTERS)?;
                Ok(data_bytes(false, values.len()).saturating_add(6))
            }
        }
    }

    /// Appends the PDU of a request that [`Request::size`] accepted.
    pub(crate) fn write_to(&self, out: &mut Vec<u8>) {
        out.push(self.function());
        put(out, self.head());
        match self {
            Self::WriteCoils { values, .. } => put_bits(out, values),
            Self::WriteRegisters { values, .. } => put_registers(out, values),
            _ => {}
        }
    }

    /// Appends a device's reply to this read, of `bits` read from coils or
    /// discrete inputs.
    pub(crate) fn reply_bits(&self, bits: &[bool], out: &mut Vec<u8>) {
        out.push(self.function());
        put_bits(out, bits);
    }

    /// Appends a device's reply to this read, of `registers` read.
    pub(crate) fn reply_registers(&self, registers: &[u16], out: &mut Vec<u8>) {
        out.push(self.function());
        put_registers(out, registers);
    }

    /// Appends a device's reply to this write: its function and the echo.
    pub(crate) fn reply_written(&self, out: &mut Vec<u8>) {
        out.push(self.function());
        put(out, self.head());
    }

    /// The two fields after the function code: the first address, then the count
    /// or the single value. A write reply echoes them.
    fn head(&self) -> [u16; 2] {
        match self {
            Self::Read { start, count, .. } => [*start, *count],
            Self::WriteCoil { address, value } => [*address, coil(*value)],
            Self::WriteRegister { address, value } => [*address, *value],
            Self::WriteCoils { start, values } => [*start, count(values)],
            Self::WriteRegisters { start, values } => [*start, count(values)],
        }
    }

    /// The length of the reply PDU to this request that starts with `head`, or
    /// `None` when `head` is empty.
    pub(crate) fn reply_len(&self, head: &[u8]) -> Option<usize> {
        let &function = head.first()?;
        Some(if function & EXCEPTION == 0 {
            self.reply_size()
        } else {
            2
        })
    }

    /// The length of a reply that is not an exception.
    fn reply_size(&self) -> usize {
        match self {
            Self::Read { table, count, .. } => {
                data_bytes(table.bits(), usize::from(*count)).saturating_add(2)
            }
            _ => 5,
        }
    }
}

/// A device's reply to a [`Request`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reply<'a> {
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
    /// `count` bits packed in `bytes`, whose unused high bits must be 0.
    fn new(bytes: &'a [u8], count: u16) -> Result<Self, Error> {
        let used = u32::from(count & 7);
        if let Some(&last) = bytes.last()
            && used != 0
            && last.checked_shr(used).unwrap_or(0) != 0
        {
            return Err(Error::Padding(last));
        }
        Ok(Self { bytes, count })
    }

    /// The number of bits.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::from(self.count)
    }

    /// Whether the reply holds no bits. A reply to a valid request never does.
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

    /// Whether the reply holds no registers. A reply to a valid request never does.
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

impl Exception {
    /// The exception a device gives for a request PDU that [`Request::decode`]
    /// refused. It is right because `decode` checks in the specification's order:
    /// the function, then every value, then the range.
    pub(crate) fn of(error: &Error) -> Self {
        match error {
            Error::Function(_) => Self::IllegalFunction,
            Error::Range { .. } => Self::IllegalAddress,
            Error::Size { .. }
            | Error::Count { .. }
            | Error::ByteCount { .. }
            | Error::Padding(_)
            | Error::Coil(_) => Self::IllegalValue,
            Error::Protocol(_)
            | Error::Length(_)
            | Error::Answer { .. }
            | Error::Echo { .. }
            | Error::Frame(_)
            | Error::Crc { .. } => {
                unreachable!("invariant: a request PDU never gives {error}")
            }
        }
    }

    /// Appends the exception reply to a request of `function`.
    pub(crate) fn write_to(self, function: u8, out: &mut Vec<u8>) {
        out.extend_from_slice(&[function | EXCEPTION, self.code()]);
    }

    fn code(self) -> u8 {
        match self {
            Self::IllegalFunction => 1,
            Self::IllegalAddress => 2,
            Self::IllegalValue => 3,
            Self::DeviceFailure => 4,
            Self::Acknowledge => 5,
            Self::Busy => 6,
            Self::GatewayPath => 10,
            Self::GatewayTarget => 11,
            Self::Other(code) => code,
        }
    }
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

/// The length of the request PDU that starts with `head`, or `None` when `head`
/// is too short to give it.
pub(crate) fn request_len(head: &[u8]) -> Result<Option<usize>, Error> {
    let Some(&function) = head.first() else {
        return Ok(None);
    };
    match function {
        READ_COILS..=WRITE_REGISTER => Ok(Some(5)),
        WRITE_COILS | WRITE_REGISTERS => Ok(head
            .get(5)
            .map(|&bytes| usize::from(bytes).saturating_add(6))),
        other => Err(Error::Function(other)),
    }
}

fn check(start: u16, count: usize, max: u16) -> Result<(), Error> {
    if count == 0 || count > usize::from(max) {
        return Err(Error::Count { count, max });
    }
    if usize::from(start).saturating_add(count) > 0x1_0000 {
        return Err(Error::Range { start, count });
    }
    Ok(())
}

/// The bytes that `count` bits or registers take in a PDU.
fn data_bytes(bits: bool, count: usize) -> usize {
    if bits {
        count.div_ceil(8)
    } else {
        count.saturating_mul(2)
    }
}

/// The count field for `values`. It saturates for a list that no check accepts,
/// so such a request's echo never matches.
fn count<T>(values: &[T]) -> u16 {
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

/// Appends the byte count and the packed `bits`.
fn put_bits(out: &mut Vec<u8>, bits: &[bool]) {
    out.push(byte_count(data_bytes(true, bits.len())));
    out.extend(bits.chunks(8).map(pack));
}

/// Appends the byte count and the `registers`.
fn put_registers(out: &mut Vec<u8>, registers: &[u16]) {
    out.push(byte_count(data_bytes(false, registers.len())));
    for register in registers {
        out.extend_from_slice(&register.to_be_bytes());
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

fn echo(pdu: &[u8], want: [u16; 2]) -> Result<Reply<'_>, Error> {
    let got = fields(pdu)?;
    if got != want {
        return Err(Error::Echo { want, got });
    }
    Ok(Reply::Written)
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
        let Ok(Reply::Bits(bits)) = coils.decode_reply(&[0x01, 0x03, 0xCD, 0x6B, 0x05])
        else {
            panic!("bits");
        };
        let want = "1011001111010110101";
        let got: String = bits.iter().map(|b| if b { '1' } else { '0' }).collect();
        assert_eq!((bits.len(), got.as_str()), (19, want));

        let inputs = read(Table::DiscreteInputs, 196, 22);
        assert_eq!(encoded(&inputs), [0x02, 0x00, 0xC4, 0x00, 0x16]);
        let Ok(Reply::Bits(bits)) =
            inputs.decode_reply(&[0x02, 0x03, 0xAC, 0xDB, 0x35])
        else {
            panic!("bits");
        };
        assert_eq!(bits.iter().filter(|&b| b).count(), 14);

        let holding = read(Table::HoldingRegisters, 107, 3);
        assert_eq!(encoded(&holding), [0x03, 0x00, 0x6B, 0x00, 0x03]);
        let reply = [0x03, 0x06, 0x02, 0x2B, 0x00, 0x00, 0x00, 0x64];
        let Ok(Reply::Registers(registers)) = holding.decode_reply(&reply) else {
            panic!("registers");
        };
        let values: Vec<u16> = registers.iter().collect();
        assert_eq!((registers.len(), values), (3, vec![0x022B, 0, 0x64]));

        let input = read(Table::InputRegisters, 8, 1);
        assert_eq!(encoded(&input), [0x04, 0x00, 0x08, 0x00, 0x01]);
        let Ok(Reply::Registers(registers)) = input.decode_reply(&[0x04, 0x02, 0, 10])
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
            assert_eq!(request.decode_reply(&reply), Ok(Reply::Written));
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
                Ok(Reply::Exception(want))
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
                    count: 70_000,
                    max: 123,
                },
            ),
        ];
        for (request, error) in cases {
            let mut out = vec![9];
            assert_eq!(request.encode(&mut out), Err(error.clone()));
            assert_eq!(out, [9], "out is unchanged");
        }
    }

    #[test]
    fn accepts_the_most_items_of_each_function() {
        let most = [
            read(Table::Coils, 0, 2000),
            read(Table::InputRegisters, 0, 125),
            Request::WriteCoils {
                start: 0,
                values: vec![true; 1968],
            },
            Request::WriteRegisters {
                start: 0,
                values: vec![1; 123],
            },
        ];
        for request in most {
            assert_eq!(request.encode(&mut Vec::new()), Ok(()), "{request:?}");
        }
    }

    #[test]
    fn says_whether_a_reply_is_empty() {
        let none = read(Table::Coils, 0, 0);
        let Ok(Reply::Bits(bits)) = none.decode_reply(&[0x01, 0x00]) else {
            panic!("bits");
        };
        assert!(bits.is_empty());
        let one = read(Table::Coils, 0, 1);
        let Ok(Reply::Bits(bits)) = one.decode_reply(&[0x01, 0x01, 0x01]) else {
            panic!("bits");
        };
        assert!(!bits.is_empty());
        let none = read(Table::HoldingRegisters, 0, 0);
        let Ok(Reply::Registers(registers)) = none.decode_reply(&[0x03, 0x00]) else {
            panic!("registers");
        };
        assert!(registers.is_empty());
        let one = read(Table::HoldingRegisters, 0, 1);
        let reply = [0x03, 0x02, 0, 1];
        let Ok(Reply::Registers(registers)) = one.decode_reply(&reply) else {
            panic!("registers");
        };
        assert!(!registers.is_empty());
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
        let cases: [(&[u8], Error, &str); 12] = [
            (
                &[0x10, 0xFF, 0xFF, 0x00, 0x02, 0x03, 0, 0, 0],
                Error::ByteCount { want: 4, got: 3 },
                "a byte count of 3 where the item count gives 4",
            ),
            (
                &[0x0F, 0xFF, 0xFF, 0x00, 0x02, 0x01, 0xFF],
                Error::Padding(0xFF),
                "a last bit byte of 0xff whose unused bits are not 0",
            ),
            (
                &[0x0F, 0x00, 0x00, 0x00, 0x01, 0x01, 0xFF],
                Error::Padding(0xFF),
                "a last bit byte of 0xff whose unused bits are not 0",
            ),
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
                Error::ByteCount { want: 4, got: 3 },
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
    fn writes_each_exception_code_it_reads() {
        for code in 0..=u8::MAX {
            let mut out = Vec::new();
            Exception::from(code).write_to(0x03, &mut out);
            assert_eq!(out, [0x83, code]);
        }
    }

    #[test]
    fn refuses_a_reply_of_another_function_or_with_padding() {
        let coils = read(Table::Coils, 0, 9);
        let registers = read(Table::InputRegisters, 0, 2);
        let write = Request::WriteRegister {
            address: 7,
            value: 1,
        };
        let cases: [(&Request, &[u8], Error, &str); 3] = [
            (
                &registers,
                &[0x81, 0x02],
                Error::Answer { want: 4, got: 0x81 },
                "a reply of function 129 to a request of function 4",
            ),
            (
                &coils,
                &[0x01, 0x02, 0x00, 0x02],
                Error::Padding(0x02),
                "a last bit byte of 0x02 whose unused bits are not 0",
            ),
            (
                &write,
                &[],
                Error::Size { want: 5, got: 0 },
                "a PDU of 0 bytes where its function gives 5",
            ),
        ];
        for (request, pdu, error, message) in cases {
            assert_eq!(request.decode_reply(pdu), Err(error.clone()), "{pdu:x?}");
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
                Error::ByteCount { want: 2, got: 1 },
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
                Error::Size { want: 6, got: 0 },
                "a PDU of 0 bytes where its function gives 6",
            ),
            (
                &registers,
                &[0x04],
                Error::Size { want: 6, got: 1 },
                "a PDU of 1 bytes where its function gives 6",
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
        let bits = (
            prop_oneof![Just(Table::Coils), Just(Table::DiscreteInputs)],
            1..=2000_u16,
        );
        let registers = (
            prop_oneof![Just(Table::HoldingRegisters), Just(Table::InputRegisters)],
            1..=125_u16,
        );
        prop_oneof![
            (prop_oneof![bits, registers], 0..=u16::MAX).prop_map(
                |((table, count), start)| {
                    read(
                        table,
                        start.min(u16::MAX.saturating_sub(count).saturating_add(1)),
                        count,
                    )
                }
            ),
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
        fn encodes_what_it_decodes(
            pdu in prop::collection::vec(any::<u8>(), 0..300),
        ) {
            if let Ok(request) = Request::decode(&pdu) {
                prop_assert_eq!(encoded(&request), pdu);
            }
        }
    }
}
