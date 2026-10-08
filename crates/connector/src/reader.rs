//! The reader settings that each out connector reads from its config.

use document::diagnostic::{Code, Diagnostic};
use document::value::Value;
use document::{Block, Document};
use hub::reader::Mode;
use types::name::{Name, Selector};
use types::time::Span;

use crate::kind;

const BAD_MODE: Code = Code::new("connector.bad-mode");
const LATEST_HOLD: Code = Code::new("connector.latest-hold");

const READER_KEYS: [&str; 3] = ["name", "mode", "hold"];

/// The reader settings of an out connector. The reader starts from now, with no
/// maximum age.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Settings {
    /// The reader's name. `None` is the connector's name, which
    /// `kind::Context::reader` gives when it opens the reader.
    pub name: Option<Name>,
    /// The channels it reads.
    pub select: Selector,
    /// Which frames it gets.
    pub mode: Mode,
    /// How long the buffer keeps samples it has not received after it closes. Zero or
    /// more, and zero when `mode` is `Latest`.
    pub hold: Span,
}

/// Reads the `select` attribute of `config` and its one `reader` block, with the
/// attributes `name`, `mode` (`"complete"` or `"latest"`, as a string or a
/// reference), and `hold`. With no `reader` block, the reader has no name, is
/// complete, and has no hold. `keys` and `blocks` are the kind's own attributes and
/// blocks. Each other key of `config` that `read` does not read gives
/// `document.unknown-attribute` or `document.unknown-block`.
///
/// # Errors
///
/// One diagnostic for each problem: an unknown key, no `select`, a value that does
/// not read, a label, attribute, or block in `reader` that it does not take, a second
/// `reader` block, a negative `hold`, and a `hold` in `latest` mode, since only a
/// complete reader holds.
///
/// # Panics
///
/// When `keys` holds `select` or `blocks` holds `reader`: the kind's lists are
/// internal, and `read` reads those two itself.
pub fn read(
    config: &Document,
    keys: &[&str],
    blocks: &[&str],
) -> Result<Settings, Vec<Diagnostic>> {
    assert!(
        !keys.contains(&"select") && !blocks.contains(&"reader"),
        "a kind lists `select` or `reader`, which `read` reads itself: keys {keys:?}, \
         blocks {blocks:?}"
    );
    let mut diagnostics = document::read::unknown(
        config,
        kind::NOUN,
        &[&["select"], keys].concat(),
        &[&["reader"], blocks].concat(),
    );
    let select = keep(
        document::read::required(
            config,
            kind::NOUN,
            None,
            "select",
            document::read::selector,
            "Add a `select` attribute with the channels it reads, such as \
             \"site_a.**\""
                .into(),
        ),
        &mut diagnostics,
    );
    diagnostics.extend(document::read::repeated(config, kind::NOUN, "reader"));
    let first = config
        .blocks
        .iter()
        .find(|block| &*block.keyword == "reader");
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
    let fix = "Remove each label, and name the reader with a `name` attribute";
    keep(document::read::labels::<0>(block, fix.into()), diagnostics);
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
    let span =
        hold.map(|hold| keep(document::read::duration(&hold.value), diagnostics));
    if let (Some(hold), Some(Some(Mode::Latest))) = (hold, mode) {
        diagnostics.push(Diagnostic::new(
            LATEST_HOLD,
            hold.key_span,
            "the reader has a `hold` in `latest` mode, and only a complete reader \
             holds"
                .into(),
            "Use `mode = \"complete\"`, or remove the `hold`".into(),
        ));
    }
    (
        name.flatten(),
        mode.flatten().unwrap_or(Mode::Complete),
        span.flatten().unwrap_or(Span::ZERO),
    )
}

/// Reads `"complete"` or `"latest"`, as a string or a reference.
fn mode(value: &Value) -> Result<Mode, Diagnostic> {
    let Some(text) = value.kind.text() else {
        return Err(Diagnostic::new(
            BAD_MODE,
            value.span,
            format!(
                "a mode is a string or a reference, not {}",
                value.kind.noun()
            ),
            "Write \"complete\" or \"latest\"".into(),
        ));
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
