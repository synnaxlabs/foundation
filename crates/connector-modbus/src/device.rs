//! A Modbus device in memory: the four tables, and the answer to each request.
//! Tests and simulations use it as the far side of a connection.

use std::ops::Range;

use crate::pdu::{Exception, Request, Table};

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
    /// One check is stricter than the specification: a function 15 request whose
    /// last data byte has an unused bit set gets `IllegalValue`, so a test catches
    /// a client that sends one.
    ///
    /// # Panics
    ///
    /// Only on a bug in this crate: a request decode error that no request PDU
    /// can cause.
    pub fn answer(&mut self, pdu: &[u8], out: &mut Vec<u8>) {
        let Some(&function) = pdu.first() else {
            return;
        };
        let exception = match Request::decode(pdu) {
            Ok(request) => match self.apply(&request, out) {
                Some(()) => return,
                None => Exception::IllegalAddress,
            },
            Err(error) => Exception::of(&error),
        };
        exception.write_to(function, out);
    }

    /// Does a valid request and appends its reply, or gives `None` with `self`
    /// and `out` unchanged when the request's range is not in the device.
    fn apply(&mut self, request: &Request, out: &mut Vec<u8>) -> Option<()> {
        match request {
            Request::Read {
                table,
                start,
                count,
            } => {
                let span = span(*start, usize::from(*count));
                match table {
                    Table::Coils => request.reply_bits(self.coils.get(span)?, out),
                    Table::DiscreteInputs => {
                        request.reply_bits(self.discrete_inputs.get(span)?, out);
                    }
                    Table::HoldingRegisters => {
                        request.reply_registers(self.holding_registers.get(span)?, out);
                    }
                    Table::InputRegisters => {
                        request.reply_registers(self.input_registers.get(span)?, out);
                    }
                }
                return Some(());
            }
            Request::WriteCoil { address, value } => {
                write(&mut self.coils, *address, &[*value])?;
            }
            Request::WriteRegister { address, value } => {
                write(&mut self.holding_registers, *address, &[*value])?;
            }
            Request::WriteCoils { start, values } => {
                write(&mut self.coils, *start, values)?;
            }
            Request::WriteRegisters { start, values } => {
                write(&mut self.holding_registers, *start, values)?;
            }
        }
        request.reply_written(out);
        Some(())
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

#[cfg(test)]
mod tests;
