//! Reads config files into Documents through the front end of each syntax.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use document::diagnostic::{Code, Diagnostic};
use document::{Document, Source};

const UNKNOWN_EXTENSION: Code = Code::new("ops.unknown-extension");

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
/// `ops.unknown-extension` for each file that no front end reads, and each problem of a
/// front end, in file order.
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
        let front_end = file
            .path
            .file_name()
            .and_then(|name| name.to_str()?.rsplit_once('.'))
            .and_then(|(_, extension)| front_ends.get(extension));
        let Some(front_end) = front_end else {
            diagnostics.push(unknown(&file.path, front_ends));
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

/// The `ops.unknown-extension` diagnostic of `path`.
pub(crate) fn unknown(
    path: &Path,
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
        format!("no config syntax reads {:?}", path.display().to_string()),
        format!("Use a file that ends in {extensions}"),
    )
}
