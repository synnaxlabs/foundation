use document::diagnostic::{Code, Diagnostic};
use document::value::Value;
use document::{Block, Span, read};
use spec::channel::{Data, Edge, Error, Kind};
use spec::data_type::DataType;
use spec::unit::Unit;
use types::name::Name;

use crate::{Definition, Found, Reported, written};

const BAD_CHANNEL_KIND: Code = Code::new("config.bad-channel-kind");
const BAD_DATA_TYPE: Code = Code::new("config.bad-data-type");
const BAD_UNIT: Code = Code::new("config.bad-unit");
const UNKNOWN_CHANNEL: Code = Code::new("config.unknown-channel");
const INDEX_KEYS: [&str; 3] = ["kind", "error", "control"];
const DATA_KEYS: [&str; 5] = ["kind", "data_type", "index", "quality", "unit"];

/// The check of the attributes of one kind of channel.
type Attributes = fn(&mut Found<'_>, &Block) -> Option<Kind<Name>>;

/// Checks a `channel` block and gives its channel, with each edge as a name. A bad
/// `kind` stops the check of each attribute but the edges, because the others depend
/// on it.
pub(crate) fn check(found: &mut Found<'_>, block: &Block) -> Option<Definition> {
    let attributes = found.attribute(block, "kind", |value| -> Result<Attributes, _> {
        match text(value, BAD_CHANNEL_KIND, "the channel kind", "\"index\"")? {
            "index" => Ok(index),
            "data" => Ok(data),
            text => Err(Diagnostic::new(
                BAD_CHANNEL_KIND,
                value.span,
                format!("{text:?} is not a kind of channel"),
                "Write \"index\" or \"data\"".into(),
            )),
        }
    });
    let Ok(attributes) = attributes else {
        // A bad kind hides which attributes are unknown, but not the blocks inside.
        let keys: Vec<&str> = block.body.attributes.iter().map(|a| &*a.key).collect();
        drop(found.unknown(block, &keys));
        for each in [Edge::Index, Edge::Quality, Edge::Error, Edge::Control] {
            drop(edge(found, block, each));
        }
        return None;
    };
    let attributes = attributes.unwrap_or(data);
    attributes(found, block).map(Definition::Channel)
}

fn index(found: &mut Found<'_>, block: &Block) -> Option<Kind<Name>> {
    let unknown = found.unknown(block, &INDEX_KEYS);
    let error = edge(found, block, Edge::Error);
    let control = edge(found, block, Edge::Control);
    let (Ok(()), Ok(error), Ok(control)) = (unknown, error, control) else {
        return None;
    };
    Some(Kind::Index { error, control })
}

fn data(found: &mut Found<'_>, block: &Block) -> Option<Kind<Name>> {
    let unknown = found.unknown(block, &DATA_KEYS);
    let index = edge(found, block, Edge::Index).and_then(|index| {
        index.ok_or_else(|| {
            let fix = "Add an `index` attribute with the name of an index channel, \
                       such as \"edge.time\"";
            found.missing(block, &["index"], fix.into());
            Reported
        })
    });
    let quality = edge(found, block, Edge::Quality);
    let data_type = found.required(
        block,
        "data_type",
        |value| {
            let text = text(value, BAD_DATA_TYPE, "the data type", "\"f64\"")?;
            text.parse::<DataType>().map_err(|error| {
                Diagnostic::new(
                    BAD_DATA_TYPE,
                    value.span,
                    format!("cannot read the data type {text:?}: {error}"),
                    error.fix().into(),
                )
            })
        },
        "Add a `data_type` attribute such as \"f64\"".into(),
    );
    let unit = found.attribute(block, "unit", |value| {
        let text = text(value, BAD_UNIT, "the unit", "\"kPa\"")?;
        Unit::new(text).map_err(|error| {
            Diagnostic::new(
                BAD_UNIT,
                value.span,
                format!("cannot read the unit {text:?}: {error}"),
                error.fix().into(),
            )
        })
    });
    let (Ok(()), Ok(index), Ok(quality), Ok(data_type), Ok(unit)) =
        (unknown, index, quality, data_type, unit)
    else {
        return None;
    };
    match Data::new(index, quality, data_type, unit) {
        Ok(data) => Some(Kind::Data(data)),
        Err(ref error @ Error::Unit { ref data_type }) => {
            found.diagnostics.push(Diagnostic::new(
                BAD_UNIT,
                span(block, "unit"),
                format!("{error}: the data type is \"{data_type}\""),
                error.fix().into(),
            ));
            None
        }
    }
}

/// Reads the attribute of `edge`. A name that no `channel` block defines gives
/// `config.unknown-channel`.
fn edge(
    found: &mut Found<'_>,
    block: &Block,
    edge: Edge,
) -> Result<Option<Name>, Reported> {
    let key = match edge {
        Edge::Index => "index",
        Edge::Quality => "quality",
        Edge::Error => "error",
        Edge::Control => "control",
    };
    match found.attribute(block, key, read::name)? {
        Some(to) if !found.channels.contains(&to) => {
            found.diagnostics.push(Diagnostic::new(
                UNKNOWN_CHANNEL,
                span(block, key),
                format!("no `channel` block defines the {edge} `{to}`"),
                "Name a channel that a `channel` block defines".into(),
            ));
            Err(Reported)
        }
        to => Ok(to),
    }
}

/// The span of the value of `key` in `block`.
fn span(block: &Block, key: &str) -> Option<Span> {
    block.body.attributes.get(key)?.value.span
}

/// The text of a string or a reference. Another value gives `code`, with `what` in
/// the message and `example` in the fix.
fn text<'v>(
    value: &'v Value,
    code: Code,
    what: &str,
    example: &str,
) -> Result<&'v str, Diagnostic> {
    written(value).ok_or_else(|| {
        Diagnostic::new(
            code,
            value.span,
            format!(
                "{what} is a string or a reference, not {}",
                value.kind.noun()
            ),
            format!("Write a string such as {example}"),
        )
    })
}
