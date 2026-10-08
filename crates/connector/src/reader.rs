//! The reader settings that each out connector reads from its config.

use document::diagnostic::{Code, Diagnostic};
use document::value::{Kind, Value};
use document::{Block, Document};
use hub::reader::Mode;
use types::name::{Name, Selector};
use types::time::Span;

const LABEL_COUNT: Code = Code::new("config.label-count");
const REPEATED_BLOCK: Code = Code::new("config.repeated-block");
const BAD_MODE: Code = Code::new("connector.bad-mode");
const LATEST_HOLD: Code = Code::new("connector.latest-hold");
const NEGATIVE_SPAN: Code = Code::new("config.negative-span");
const UNNAMED_HOLD: Code = Code::new("connector.unnamed-hold");

const READER_KEYS: [&str; 3] = ["name", "mode", "hold"];

/// The reader settings of an out connector. The reader starts from now, with no
/// maximum age.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Settings {
    /// The reader's name. `None` is an ad hoc reader, which holds nothing.
    pub name: Option<Name>,
    /// The channels it reads.
    pub select: Selector,
    /// Which frames it gets.
    pub mode: Mode,
    /// How long the buffer keeps samples it has not received after it closes. Zero or
    /// more, and zero when `name` is `None` or `mode` is `Latest`.
    pub hold: Span,
}

/// Reads the `select` attribute of `config` and its one `reader` block, with the
/// attributes `name`, `mode` (`"complete"` or `"latest"`, as a string or a
/// reference), and `hold`. With no `reader` block, the reader is ad hoc and complete,
/// with no hold. `keys` and `blocks` are the kind's own attributes and blocks. Each
/// other key of `config` that `read` does not read gives
/// `document.unknown-attribute` or `document.unknown-block`.
///
/// # Errors
///
/// One diagnostic for each problem: an unknown key, no `select`, a value that does
/// not read, a label, attribute, or block in `reader` that it does not take, a second
/// `reader` block, a negative `hold`, and a `hold` with no `name` or in `latest` mode,
/// since only a named complete reader holds.
pub fn read(
    config: &Document,
    keys: &[&str],
    blocks: &[&str],
) -> Result<Settings, Vec<Diagnostic>> {
    let mut diagnostics = document::read::unknown(
        config,
        "the connector",
        &with("select", keys),
        &with("reader", blocks),
    );
    let select = keep(
        document::read::required(
            config,
            "the connector",
            None,
            "select",
            document::read::selector,
            "Add a `select` attribute with the channels it reads, such as \
             \"site_a.**\""
                .into(),
        ),
        &mut diagnostics,
    );
    let mut readers = config
        .blocks
        .iter()
        .filter(|block| &*block.keyword == "reader");
    let first = readers.next();
    for repeated in readers {
        diagnostics.push(Diagnostic::new(
            REPEATED_BLOCK,
            repeated.keyword_span,
            "the connector has a second `reader` block".into(),
            "Join the two into one".into(),
        ));
    }
    let (name, mode, hold) = match first {
        Some(first) => block(first, &mut diagnostics),
        None => (None, Mode::Complete, Span::ZERO),
    };
    match select {
        Some(select) if diagnostics.is_empty() => Ok(Settings {
            name,
            select,
            mode,
            hold,
        }),
        _ => Err(diagnostics),
    }
}

/// The name, mode, and hold of the `reader` block. Each value that it reports gives
/// its default.
fn block(
    block: &Block,
    diagnostics: &mut Vec<Diagnostic>,
) -> (Option<Name>, Mode, Span) {
    if let Some(label) = block.labels.first() {
        diagnostics.push(Diagnostic::new(
            LABEL_COUNT,
            label.span,
            "the `reader` block takes no label".into(),
            "Remove each label, and name the reader with a `name` attribute".into(),
        ));
    }
    diagnostics.extend(document::read::unknown(
        &block.body,
        "the `reader` block",
        &READER_KEYS,
        &[],
    ));
    let attribute = |key: &str| block.body.attributes.get(key);
    let name = attribute("name")
        .map(|name| keep(document::read::name(&name.value), diagnostics));
    let mode = attribute("mode").map(|mode| keep(self::mode(&mode.value), diagnostics));
    let hold = attribute("hold");
    let span = hold.map(|hold| keep(self::hold(&hold.value), diagnostics));
    if let Some(hold) = hold {
        let at = hold.key_span;
        if name.is_none() {
            diagnostics.push(Diagnostic::new(
                UNNAMED_HOLD,
                at,
                "the reader has a `hold` and no `name`, and an ad hoc reader holds \
                 nothing"
                    .into(),
                "Add a `name`, or remove the `hold`".into(),
            ));
        }
        if let Some(Some(Mode::Latest)) = mode {
            diagnostics.push(Diagnostic::new(
                LATEST_HOLD,
                at,
                "the reader has a `hold` in `latest` mode, and only a complete \
                 reader holds"
                    .into(),
                "Use `mode = \"complete\"`, or remove the `hold`".into(),
            ));
        }
    }
    (
        name.flatten(),
        mode.flatten().unwrap_or(Mode::Complete),
        span.flatten().unwrap_or(Span::ZERO),
    )
}

/// Reads a span of zero or more.
fn hold(value: &Value) -> Result<Span, Diagnostic> {
    let span = document::read::span(value)?;
    if span < Span::ZERO {
        return Err(Diagnostic::new(
            NEGATIVE_SPAN,
            value.span,
            format!("the reader holds {span}, which is below zero"),
            "Write a hold of zero or more".into(),
        ));
    }
    Ok(span)
}

/// Reads `"complete"` or `"latest"`, as a string or a reference.
fn mode(value: &Value) -> Result<Mode, Diagnostic> {
    let text = match &value.kind {
        Kind::String(text) => text,
        Kind::Reference(name) => name.as_str(),
        kind => {
            return Err(Diagnostic::new(
                BAD_MODE,
                value.span,
                format!("a mode is a string or a reference, not {}", kind.noun()),
                "Write \"complete\" or \"latest\"".into(),
            ));
        }
    };
    match text {
        "complete" => Ok(Mode::Complete),
        "latest" => Ok(Mode::Latest),
        _ => Err(Diagnostic::new(
            BAD_MODE,
            value.span,
            format!("the reader has no mode {text:?}"),
            "Write \"complete\" or \"latest\"".into(),
        )),
    }
}

/// `own` and then each of `keys` that is not `own`.
fn with<'a>(own: &'a str, keys: &[&'a str]) -> Vec<&'a str> {
    let others = keys.iter().copied().filter(|key| *key != own);
    std::iter::once(own).chain(others).collect()
}

/// The value of `result`, or `None` after it adds the diagnostic.
fn keep<T>(
    result: Result<T, Diagnostic>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<T> {
    result
        .map_err(|diagnostic| diagnostics.push(diagnostic))
        .ok()
}

#[cfg(test)]
mod tests;
