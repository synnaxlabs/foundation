//! The config of one `modbus` connector, read from its document.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use connector::kind;
use document::diagnostic::{Code, Diagnostic, Note};
use document::schema::{self, Attribute, Body, Presence};
use document::value::{Kind as Of, Value};
use document::{Block, Document, read};
use env::net::host::{self, Host};
use types::name::Name;
use types::time::Rate;

use crate::pdu::Table;
use crate::read::Order;

#[cfg(test)]
mod tests;

const BAD_ADDRESS: Code = Code::new("modbus.bad-address");
const BAD_UNIT: Code = Code::new("modbus.bad-unit");
const BAD_ORDER: Code = Code::new("modbus.bad-order");
const BAD_TABLE: Code = Code::new("modbus.bad-table");
const BAD_READ_ADDRESS: Code = Code::new("modbus.bad-read-address");
const NO_READ: Code = Code::new("modbus.no-read");
const REPEATED_CHANNEL: Code = Code::new("modbus.repeated-channel");

/// The port of Modbus TCP.
const PORT: u16 = 502;

/// The word of each table.
const TABLES: [(&str, Table); 4] = [
    ("coil", Table::Coils),
    ("discrete", Table::DiscreteInputs),
    ("holding", Table::HoldingRegisters),
    ("input", Table::InputRegisters),
];

/// The word of each order.
const ORDERS: [(&str, Order); 2] = [("big", Order::Big), ("little", Order::Little)];

/// The body of the config.
pub(crate) const BODY: Body = Body {
    summary: "Polls the registers and coils of a Modbus TCP device at a rate.",
    attributes: &[
        Attribute {
            name: "address",
            value: "a host and an optional port, such as \"plc:502\"",
            presence: Presence::Required,
            summary: "The endpoint of the device. Port 502 when it has none.",
        },
        Attribute {
            name: "order",
            value: "\"big\" or \"little\"",
            presence: Presence::Default("\"big\""),
            summary: "The word order of a value that spans registers.",
        },
        Attribute {
            name: "rate",
            value: "a rate, such as \"10Hz\"",
            presence: Presence::Required,
            summary: "The reads per second.",
        },
        Attribute {
            name: "unit",
            value: "an integer, 0 to 255",
            presence: Presence::Default("1"),
            summary: "The unit of the device.",
        },
    ],
    blocks: &[schema::Block {
        keyword: "read",
        label: Some("the channel"),
        forms: &[READ],
    }],
    examples: &[EXAMPLE],
};

/// A connector to one device.
const EXAMPLE: &str = "\
address = \"plc:502\"
rate    = \"10Hz\"
unit    = 1
order   = \"big\"

read \"plant.pressure\" {
  table   = \"holding\"
  address = 0
}
";

/// The body of a `read` block.
const READ: Body = Body {
    summary: "Where the value of one channel is in the device. One or more.",
    attributes: &[
        Attribute {
            name: "address",
            value: "an integer, 0 to 65535",
            presence: Presence::Required,
            summary: "The address of the value in its table, from 0.",
        },
        Attribute {
            name: "table",
            value: "\"coil\", \"discrete\", \"holding\", or \"input\"",
            presence: Presence::Required,
            summary: "The table of the value.",
        },
    ],
    blocks: &[],
    examples: &[],
};

/// One modbus connector's config: the endpoint and unit of the device, the rate, the
/// word order, and the read of each channel. Only
/// [`Kind::parse`](connector::kind::Kind::parse) of [`Kind`](crate::Kind) makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub(crate) host: Host,
    pub(crate) port: u16,
    pub(crate) unit: u8,
    pub(crate) rate: Rate,
    pub(crate) order: Order,
    /// One or more, each of another channel.
    pub(crate) reads: Vec<Read>,
}

/// Where the value of one channel is in the device.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Read {
    pub(crate) channel: Name,
    pub(crate) table: Table,
    pub(crate) address: u16,
}

/// Reads `address`, `rate`, the optional `unit` and `order`, and one or more `read`
/// blocks, and no other key or block.
pub(crate) fn parse(config: &Document) -> Result<Config, Vec<Diagnostic>> {
    let mut diagnostics = Vec::new();
    let endpoint = read::required(
        config,
        kind::NOUN,
        None,
        "address",
        endpoint,
        "Add an `address` attribute with the host and port of the device, such as \
         \"plc:502\""
            .into(),
    );
    let rate = read::required(
        config,
        kind::NOUN,
        None,
        "rate",
        read::rate,
        "Add a `rate` attribute with the reads per second, such as \"10Hz\"".into(),
    );
    let attribute = |key| config.attributes.get(key).map(|attribute| &attribute.value);
    let unit = attribute("unit").map_or(Ok(1), unit);
    let order = attribute("order").map_or(Ok(Order::Big), order);
    let endpoint = endpoint.map_err(|error| diagnostics.push(error));
    let rate = rate.map_err(|error| diagnostics.push(error));
    let unit = unit.map_err(|error| diagnostics.push(error));
    let order = order.map_err(|error| diagnostics.push(error));
    let reads = reads(config, &mut diagnostics);
    diagnostics.extend(read::unknown(config, kind::NOUN, &BODY));
    match (endpoint, rate, unit, order) {
        (Ok((host, port)), Ok(rate), Ok(unit), Ok(order)) if diagnostics.is_empty() => {
            Ok(Config {
                host,
                port,
                unit,
                rate,
                order,
                reads,
            })
        }
        _ => Err(diagnostics),
    }
}

/// Reads a host and an optional port, such as `"plc:502"`, `"plc"`, or
/// `"[fd00::1]:502"`. Port 502 when it has none. The host is an
/// [`env::net::host::Host`]. An IPv6 host is in brackets, and the text in them is
/// parsed bare.
fn endpoint(value: &Value) -> Result<(Host, u16), Diagnostic> {
    let message = match &value.kind {
        Of::String(text) => match address(text) {
            Ok(endpoint) => return Ok(endpoint),
            Err(message) => message,
        },
        other => format!("an address is a string, not {}", other.noun()),
    };
    Err(Diagnostic::new(
        BAD_ADDRESS,
        value.span,
        message,
        "Write a host and a port, such as \"plc:502\" or \"[fd00::1]:502\"".into(),
    ))
}

/// The host and port of the address `text`, or the message of its problem.
fn address(text: &str) -> Result<(Host, u16), String> {
    let (host, port) = split(text)?;
    let host = host.parse().map_err(|problem: host::Error| {
        format!("the address {text:?} has {problem}")
    })?;
    let port = match port {
        None => PORT,
        Some(digits) => match digits.parse() {
            Ok(port)
                if port != 0 && digits.bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                port
            }
            _ => {
                return Err(format!(
                    "the address {text:?} has a port that is not 1 to 65535"
                ));
            }
        },
    };
    Ok((host, port))
}

/// The host of the address `text`, with no brackets, and the text of its port when it
/// has one. A host in brackets must hold a `:`.
fn split(text: &str) -> Result<(&str, Option<&str>), String> {
    let Some(rest) = text.strip_prefix('[') else {
        return match text.split_once(':') {
            Some((_, port)) if port.contains(':') => Err(format!(
                "the address {text:?} has more than one `:`. Put an IPv6 host in \
                 brackets"
            )),
            Some((host, port)) => Ok((host, Some(port))),
            None => Ok((text, None)),
        };
    };
    let Some((host, port)) = rest.split_once(']') else {
        return Err(format!("the address {text:?} has no `]`"));
    };
    if !host.contains(':') {
        return Err(format!("the address {text:?} has {}", host::Error::Ipv6));
    }
    if port.is_empty() {
        return Ok((host, None));
    }
    match port.strip_prefix(':') {
        Some(port) => Ok((host, Some(port))),
        None => Err(format!(
            "the address {text:?} has {port:?} after its `]`, not a `:` and a port"
        )),
    }
}

/// Reads the unit of the device: a whole number, 0 to 255.
fn unit(value: &Value) -> Result<u8, Diagnostic> {
    let message = match &value.kind {
        Of::Integer(unit) => match u8::try_from(*unit) {
            Ok(unit) => return Ok(unit),
            Err(_) => format!("the unit {unit} is not 0 to 255"),
        },
        other => format!("a unit is an integer, not {}", other.noun()),
    };
    Err(Diagnostic::new(
        BAD_UNIT,
        value.span,
        message,
        "Write the unit of the device, 0 to 255, such as 1".into(),
    ))
}

/// Reads a word order: `big` or `little`.
fn order(value: &Value) -> Result<Order, Diagnostic> {
    word(value, &ORDERS, "an order").map_err(|message| {
        Diagnostic::new(
            BAD_ORDER,
            value.span,
            message,
            "Use `big` when the first register holds the high word, or `little`".into(),
        )
    })
}

/// Reads a table: `coil`, `discrete`, `holding`, or `input`.
fn table(value: &Value) -> Result<Table, Diagnostic> {
    word(value, &TABLES, "a table").map_err(|message| {
        let words = TABLES.map(|(word, _)| word);
        let fix = format!("Use {}", read::one_of(&words));
        Diagnostic::new(BAD_TABLE, value.span, message, fix)
    })
}

/// The item of the word that `value` holds, as a string or a reference, or the message
/// of a value that is not one of `items`, each `noun`, such as "a table".
fn word<T: Copy>(value: &Value, items: &[(&str, T)], noun: &str) -> Result<T, String> {
    let Some(text) = value.kind.text() else {
        return Err(format!(
            "{noun} is a string or a reference, not {}",
            value.kind.noun()
        ));
    };
    let found = items.iter().find(|(word, _)| text == *word);
    found
        .map(|&(_, item)| item)
        .ok_or_else(|| format!("{text:?} is not {noun}"))
}

/// Reads the address of a read: a whole number, 0 to 65535.
fn read_address(value: &Value) -> Result<u16, Diagnostic> {
    let message = match &value.kind {
        Of::Integer(address) => match u16::try_from(*address) {
            Ok(address) => return Ok(address),
            Err(_) => format!("the address {address} is not 0 to 65535"),
        },
        other => format!("an address is an integer, not {}", other.noun()),
    };
    Err(Diagnostic::new(
        BAD_READ_ADDRESS,
        value.span,
        message,
        "Write the address of the protocol, from 0, such as 40 for the 41st holding \
         register"
            .into(),
    ))
}

/// Reads the `read` blocks of `config`, and adds each problem to `diagnostics`.
fn reads(config: &Document, diagnostics: &mut Vec<Diagnostic>) -> Vec<Read> {
    const OF: &str = "the `read` block";
    let blocks = config.blocks.iter();
    let blocks: Vec<&Block> =
        blocks.filter(|block| &*block.keyword == "read").collect();
    if blocks.is_empty() {
        diagnostics.push(Diagnostic::new(
            NO_READ,
            None,
            "the connector has no `read` block".into(),
            "Add a `read` block for each channel, such as `read \"plc.temp\" { table = \
             \"holding\", address = 40 }`"
                .into(),
        ));
    }
    let mut channels: BTreeMap<Name, &Block> = BTreeMap::new();
    let mut reads = Vec::with_capacity(blocks.len());
    for block in blocks {
        let fix = "Name the channel of the block, such as `read \"plc.temp\"`";
        let channel = read::labels::<1>(block, fix.into())
            .and_then(|[label]| read::label(label))
            .map_err(|error| diagnostics.push(error));
        let table = read::required(
            &block.body,
            OF,
            block.keyword_span,
            "table",
            table,
            "Add a `table` attribute with the table of the value, such as \"holding\""
                .into(),
        );
        let table = table.map_err(|error| diagnostics.push(error));
        let address = read::required(
            &block.body,
            OF,
            block.keyword_span,
            "address",
            read_address,
            "Add an `address` attribute with the address of the value, such as 40"
                .into(),
        );
        let address = address.map_err(|error| diagnostics.push(error));
        diagnostics.extend(read::unknown(&block.body, OF, &READ));
        let Ok(channel) = channel else { continue };
        match channels.entry(channel) {
            Entry::Occupied(first) => {
                diagnostics.push(repeated(first.key(), block, first.get()));
            }
            Entry::Vacant(slot) => {
                if let (Ok(table), Ok(address)) = (table, address) {
                    reads.push(Read {
                        channel: slot.key().clone(),
                        table,
                        address,
                    });
                }
                slot.insert(block);
            }
        }
    }
    reads
}

/// The diagnostic of a `read` block of `channel` after the block `first` of it.
fn repeated(channel: &Name, block: &Block, first: &Block) -> Diagnostic {
    let label = |block: &Block| block.labels.first().and_then(|label| label.span);
    let mut diagnostic = Diagnostic::new(
        REPEATED_CHANNEL,
        label(block),
        format!("the connector reads `{channel}` in another `read` block"),
        "Remove one of the two `read` blocks".into(),
    );
    diagnostic.notes.extend(label(first).map(|span| Note {
        span,
        text: format!("the first `read` block of `{channel}`"),
    }));
    diagnostic
}
