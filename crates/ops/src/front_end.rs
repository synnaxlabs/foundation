//! Reads config files into Documents through the front end of each syntax.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use document::diagnostic::{Code, Diagnostic};
use document::{Document, Source};

use crate::error;

const UNKNOWN_EXTENSION: Code = Code::new("ops.unknown-extension");
const PATH_NOT_UTF8: Code = Code::new("ops.path-not-utf8");

/// One syntax of config files.
#[derive(Clone, Copy, Debug)]
pub struct FrontEnd {
    /// Reads the text of one file into a Document whose spans hold `source`.
    ///
    /// # Errors
    ///
    /// Each problem in the text, with its span: at least one.
    pub read: fn(source: Source, text: &str) -> Result<Document, Vec<Diagnostic>>,
}

/// One config file: its path, as the user or a directory listing gave it, and its text.
pub(crate) struct File {
    pub(crate) path: PathBuf,
    pub(crate) text: String,
}

/// Reads each file with the front end that the text after the last `.` of its file
/// name picks. `Source(i)` is `files[i]`.
///
/// # Errors
///
/// `ops.path-not-utf8` for each file whose path is not UTF-8, `ops.unknown-extension`
/// for each other file that no front end reads, and each problem of a front end, in
/// file order.
///
/// # Panics
///
/// When `front_ends` is empty, or a front end gives an error with no problem.
pub(crate) fn read(
    files: &[File],
    front_ends: &BTreeMap<&'static str, FrontEnd>,
) -> Result<Vec<Document>, Vec<Diagnostic>> {
    assert!(
        !front_ends.is_empty(),
        "invariant: `node` gives a front end"
    );
    let mut documents = Vec::new();
    let mut diagnostics = Vec::new();
    for (file, source) in files.iter().zip(0..) {
        let Some(path) = file.path.to_str() else {
            diagnostics.push(not_utf8(&file.path));
            continue;
        };
        let front_end = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str()?.rsplit_once('.'))
            .and_then(|(_, extension)| front_ends.get(extension));
        let Some(front_end) = front_end else {
            diagnostics.push(unknown(path, front_ends));
            continue;
        };
        match (front_end.read)(Source(source), &file.text) {
            Ok(document) => documents.push(document),
            Err(problems) => {
                assert!(
                    !problems.is_empty(),
                    "invariant: a front end gives a problem with each error"
                );
                diagnostics.extend(problems);
            }
        }
    }
    if diagnostics.is_empty() {
        Ok(documents)
    } else {
        Err(diagnostics)
    }
}

/// The `ops.path-not-utf8` diagnostic of `path`.
#[expect(
    clippy::unnecessary_debug_formatting,
    reason = "`Debug` quotes the path and escapes each byte that is not UTF-8"
)]
pub(crate) fn not_utf8(path: &Path) -> Diagnostic {
    Diagnostic::new(
        PATH_NOT_UTF8,
        None,
        format!("the path {path:?} is not UTF-8"),
        "Rename the file to a UTF-8 name".to_owned(),
    )
}

/// The `ops.unknown-extension` diagnostic of `path`. Its message writes the path as
/// the text of a place does, escaped, so a bidirectional control in a file name does
/// not reach the terminal.
pub(crate) fn unknown(
    path: &str,
    front_ends: &BTreeMap<&'static str, FrontEnd>,
) -> Diagnostic {
    let extensions: Vec<String> = front_ends
        .keys()
        .map(|extension| format!("`.{extension}`"))
        .collect();
    let extensions = match extensions.as_slice() {
        [one] => one.clone(),
        [first, second] => format!("{first} or {second}"),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
        [] => unreachable!("invariant: `read` checks the table"),
    };
    Diagnostic::new(
        UNKNOWN_EXTENSION,
        None,
        format!("no config syntax reads `{}`", error::escape(path)),
        format!("Use a file that ends in {extensions}"),
    )
}
