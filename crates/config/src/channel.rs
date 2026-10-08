use document::diagnostic::{Code, Diagnostic};
use document::value::{self, Value};
use document::{Block, read};
use spec::channel::{Data, DataType, Edge, Error, Kind};
use spec::unit::Unit;
use types::name::Name;

use crate::{Definition, Found, Reported};

const BAD_CHANNEL_KIND: Code = Code::new("config.bad-channel-kind");
const BAD_DATA_TYPE: Code = Code::new("config.bad-data-type");
const BAD_UNIT: Code = Code::new("config.bad-unit");
const UNKNOWN_CHANNEL: Code = Code::new("config.unknown-channel");
const INDEX_KEYS: [&str; 3] = ["kind", "error", "control"];
const DATA_KEYS: [&str; 5] = ["kind", "data_type", "index", "quality", "unit"];

/// The check of the attributes of one kind of channel.
type Read = fn(&mut Found<'_>, &Block) -> Option<Kind<Name>>;

/// Checks a `channel` block and gives its channel, with each edge as a name. A bad
/// `kind` stops the check, because the other attributes depend on it.
pub(crate) fn check(found: &mut Found<'_>, block: &Block) -> Option<Definition> {
    found.unknown_blocks(block);
    let read = found.attribute(block, "kind", |value| -> Result<Read, _> {
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
    let kind = read.ok()?.unwrap_or(data)(found, block)?;
    edges(found, block, &kind).ok()?;
    Some(Definition::Channel(kind))
}

fn index(found: &mut Found<'_>, block: &Block) -> Option<Kind<Name>> {
    let unknown = found.unknown_attributes(block, &INDEX_KEYS);
    let error = found.attribute(block, "error", read::name);
    let control = found.attribute(block, "control", read::name);
    let (Ok(()), Ok(error), Ok(control)) = (unknown, error, control) else {
        return None;
    };
    Some(Kind::Index { error, control })
}

fn data(found: &mut Found<'_>, block: &Block) -> Option<Kind<Name>> {
    let unknown = found.unknown_attributes(block, &DATA_KEYS);
    let index = found.required(
        block,
        "index",
        read::name,
        "Add an `index` attribute with the name of an index channel, such as \
         \"edge.time\""
            .into(),
    );
    let quality = found.attribute(block, "quality", read::name);
    let data_type = found.required(
        block,
        "data_type",
        |value| {
            let text = text(value, BAD_DATA_TYPE, "the data type", "\"f64\"")?;
            text.parse::<DataType>()
                .map_err(|error| refuse(block, &error))
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
        Err(error) => {
            found.diagnostics.push(refuse(block, &error));
            None
        }
    }
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

/// The text of a string or a reference, as the file wrote it.
fn written(value: &Value) -> Option<&str> {
    match &value.kind {
        value::Kind::String(text) => Some(text),
        value::Kind::Reference(name) => Some(name.as_str()),
        _ => None,
    }
}

/// The diagnostic of a channel that `block` defines and `spec` refuses.
fn refuse(block: &Block, error: &Error) -> Diagnostic {
    let value = |key| {
        block
            .body
            .attributes
            .get(key)
            .map(|attribute| &attribute.value)
    };
    match error {
        Error::Unit { data_type } => Diagnostic::new(
            BAD_UNIT,
            value("unit").and_then(|value| value.span),
            format!("{error}: the data type is \"{data_type}\""),
            error.fix().into(),
        ),
        Error::DataType(_) => {
            let value = value("data_type").expect("invariant: a data type was read");
            let text =
                written(value).expect("invariant: a data type is read from text");
            Diagnostic::new(
                BAD_DATA_TYPE,
                value.span,
                format!("cannot read the data type {text:?}: {error}"),
                error.fix().into(),
            )
        }
    }
}

/// Reports each edge of `kind` that names no channel of a `channel` block.
fn edges(
    found: &mut Found<'_>,
    block: &Block,
    kind: &Kind<Name>,
) -> Result<(), Reported> {
    let [label] = block.labels.as_slice() else {
        return Err(Reported);
    };
    let mut result = Ok(());
    for (edge, to) in kind.edges() {
        if found.channels.contains(to) {
            continue;
        }
        let key = match edge {
            Edge::Index => "index",
            Edge::Quality => "quality",
            Edge::Error => "error",
            Edge::Control => "control",
        };
        let span = block
            .body
            .attributes
            .get(key)
            .and_then(|key| key.value.span);
        found.diagnostics.push(Diagnostic::new(
            UNKNOWN_CHANNEL,
            span,
            format!(
                "the {edge} of `{}` is `{to}`, which no `channel` block defines",
                label.text
            ),
            "Name a channel that a `channel` block defines".into(),
        ));
        result = Err(Reported);
    }
    result
}
