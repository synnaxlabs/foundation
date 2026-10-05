//! A Modbus device in memory: the four tables, and the answer to each request.
//! Tests and simulations use it as the far side of a connection.

use std::ops::Range;

use crate::Error;
use crate::pdu::{self, Request, Table};

const ILLEGAL_FUNCTION: u8 = 1;
const ILLEGAL_ADDRESS: u8 = 2;
const ILLEGAL_VALUE: u8 = 3;

/// The four tables of one device. An address at or above a table's length is not
/// in the device.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Device {
    /// Read-write bits.
    pub coils: Vec<bool>,
    /// Read-only bits.
    pub discrete_inputs: Vec<bool>,
    /// Read-write 16-bit registers.
    pub holding_registers: Vec<u16>,
    /// Read-only 16-bit registers.
    pub input_registers: Vec<u16>,
}

impl Device {
    /// Does the request in `pdu` and appends the reply PDU to `out`.
    ///
    /// A request that is not valid gets an exception reply. The checks run in the
    /// order of the specification: an unknown function gives `IllegalFunction`; a
    /// wrong length, count, byte count, padding, or coil value gives
    /// `IllegalValue`; a range past address 65535 or past a table's length gives
    /// `IllegalAddress`. A write that gets an exception changes nothing. An empty
    /// PDU has no function to answer, so `out` is unchanged.
    ///
    /// # Panics
    ///
    /// Only on a bug in this crate: a request decode error that no request PDU
    /// can cause.
    pub fn answer(&mut self, pdu: &[u8], out: &mut Vec<u8>) {
        let Some(&function) = pdu.first() else {
            return;
        };
        let code = match Request::decode(pdu) {
            Ok(request) => match self.apply(function, &request, out) {
                Some(()) => return,
                None => ILLEGAL_ADDRESS,
            },
            Err(error) => exception(&error),
        };
        out.extend_from_slice(&[function | pdu::EXCEPTION, code]);
    }

    /// Does a valid request and appends its reply, or gives `None` with `self`
    /// and `out` unchanged when the request's range is not in the device.
    fn apply(
        &mut self,
        function: u8,
        request: &Request,
        out: &mut Vec<u8>,
    ) -> Option<()> {
        let fields = match request {
            Request::Read {
                table,
                start,
                count,
            } => {
                let span = span(*start, usize::from(*count));
                match table {
                    Table::Coils => read_bits(function, self.coils.get(span)?, out),
                    Table::DiscreteInputs => {
                        read_bits(function, self.discrete_inputs.get(span)?, out);
                    }
                    Table::HoldingRegisters => {
                        read_registers(
                            function,
                            self.holding_registers.get(span)?,
                            out,
                        );
                    }
                    Table::InputRegisters => {
                        read_registers(function, self.input_registers.get(span)?, out);
                    }
                }
                return Some(());
            }
            Request::WriteCoil { address, value } => {
                *self.coils.get_mut(usize::from(*address))? = *value;
                [*address, pdu::coil(*value)]
            }
            Request::WriteRegister { address, value } => {
                *self.holding_registers.get_mut(usize::from(*address))? = *value;
                [*address, *value]
            }
            Request::WriteCoils { start, values } => {
                write(&mut self.coils, *start, values)?;
                [*start, pdu::count(values)]
            }
            Request::WriteRegisters { start, values } => {
                write(&mut self.holding_registers, *start, values)?;
                [*start, pdu::count(values)]
            }
        };
        out.push(function);
        pdu::put(out, fields);
        Some(())
    }
}

/// The exception code the specification gives for a request that is not valid.
fn exception(error: &Error) -> u8 {
    match error {
        Error::Function(_) => ILLEGAL_FUNCTION,
        Error::Range { .. } => ILLEGAL_ADDRESS,
        Error::Size { .. }
        | Error::Count { .. }
        | Error::ByteCount { .. }
        | Error::Padding(_)
        | Error::Coil(_) => ILLEGAL_VALUE,
        Error::Protocol(_)
        | Error::Length(_)
        | Error::Answer { .. }
        | Error::Echo { .. } => {
            unreachable!("invariant: a request PDU never gives {error}")
        }
    }
}

fn span(start: u16, count: usize) -> Range<usize> {
    let start = usize::from(start);
    start..start.saturating_add(count)
}

fn write<T: Copy>(table: &mut [T], start: u16, values: &[T]) -> Option<()> {
    table
        .get_mut(span(start, values.len()))?
        .copy_from_slice(values);
    Some(())
}

fn read_bits(function: u8, bits: &[bool], out: &mut Vec<u8>) {
    out.push(function);
    out.push(pdu::byte_count(pdu::data_bytes(true, bits.len())));
    out.extend(bits.chunks(8).map(pdu::pack));
}

fn read_registers(function: u8, registers: &[u16], out: &mut Vec<u8>) {
    out.push(function);
    out.push(pdu::byte_count(pdu::data_bytes(false, registers.len())));
    for register in registers {
        out.extend_from_slice(&register.to_be_bytes());
    }
}

#[cfg(test)]
mod tests;
