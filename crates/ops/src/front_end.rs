//! Reads config files into Documents through the front end of each syntax.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use document::diagnostic::{Code, Diagnostic};
use document::{Document, Position, Source, Span};

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

/// The front end of each file extension that a node reads. It holds at least one.
#[derive(Clone, Debug)]
pub struct FrontEnds(BTreeMap<&'static str, FrontEnd>);

impl FrontEnds {
    /// Reads each file whose name ends in `.<extension>` with `front_end`.
    /// `extension` has no dot.
    #[must_use]
    pub fn new(extension: &'static str, front_end: FrontEnd) -> Self {
        Self(BTreeMap::from([(extension, front_end)]))
    }

    /// Adds `extension` with `front_end`, in place of the front end that
    /// `extension` had.
    #[must_use]
    pub fn with(mut self, extension: &'static str, front_end: FrontEnd) -> Self {
        self.0.insert(extension, front_end);
        self
    }

    /// Each extension, in order.
    pub(crate) fn extensions(&self) -> impl Iterator<Item = &'static str> {
        self.0.keys().copied()
    }
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
/// at the start of each other file that no front end reads, and each problem of a front
/// end, in file order.
///
/// # Panics
///
/// When a front end gives an error with no problem.
pub(crate) fn read(
    files: &[File],
    front_ends: &FrontEnds,
) -> Result<Vec<Document>, Vec<Diagnostic>> {
    let mut documents = Vec::new();
    let mut diagnostics = Vec::new();
    for (file, source) in files.iter().zip(0..) {
        let Some(path) = file.path.to_str() else {
            diagnostics.push(not_utf8(&file.path));
            continue;
        };
        let front_end = Path::new(path)
            .file_name()
            .map(|name| {
                name.to_str()
                    .expect("invariant: a part of a UTF-8 path is UTF-8")
            })
            .and_then(|name| name.rsplit_once('.'))
            .and_then(|(_, extension)| front_ends.0.get(extension));
        let Some(front_end) = front_end else {
            diagnostics.push(unknown(Source(source), front_ends));
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

/// The `ops.unknown-extension` diagnostic, at the empty span at the start of `source`.
pub(crate) fn unknown(source: Source, front_ends: &FrontEnds) -> Diagnostic {
    let extensions: Vec<String> = front_ends
        .extensions()
        .map(|extension| format!("`.{extension}`"))
        .collect();
    let extensions = match extensions.as_slice() {
        [one] => one.clone(),
        [first, second] => format!("{first} or {second}"),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
        [] => unreachable!("invariant: `FrontEnds` holds a front end"),
    };
    let start = Position {
        offset: 0,
        line: 0,
        column: 0,
    };
    Diagnostic::new(
        UNKNOWN_EXTENSION,
        Span::new(source, start, start),
        "no config syntax reads this file".to_owned(),
        format!("Use a file that ends in {extensions}"),
    )
}
