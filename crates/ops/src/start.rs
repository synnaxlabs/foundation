use std::io::Write;
use std::path::PathBuf;

use document::diagnostic::Code;
use serde_json::Value;
use types::name::Name;

use crate::error::{self, Error};

/// The arguments of `foundation start`.
#[derive(Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Start {
    /// `--data`, else `foundation-data` in the working directory.
    pub data: PathBuf,
    /// `--json`: each line is JSON.
    pub json: bool,
    /// `--name`: the first start on a data directory needs it, and a later start reads
    /// it from there.
    pub name: Option<Name>,
}

/// Why a node did not start, or stopped with an error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// What kind of failure it is, such as `node.busy`.
    pub code: Code,
    /// What went wrong.
    pub message: String,
    /// What to do about it.
    pub fix: String,
}

impl Start {
    /// The line that tells that the node `name` runs in [`Start::data`], as text or
    /// JSON, with its newline. Each gives the data directory lossily, as
    /// `Path::to_string_lossy` does.
    /// Text also escapes it as [`Start::fail`] does: the line stays one line, and a
    /// literal `\n` in the path reads apart from a newline.
    #[must_use]
    pub fn line(&self, name: &Name) -> String {
        if self.json {
            // By hand, as a `json!` object sorts its keys.
            let name = Value::from(name.as_str());
            let data = Value::from(self.data.to_string_lossy());
            format!("{{\"name\":{name},\"data\":{data}}}\n")
        } else {
            let data = error::escape(&self.data.to_string_lossy());
            format!("node {name} runs in {data}. Stop it with Ctrl-C.\n")
        }
    }

    /// Writes `failure` to `errors` as [`crate::cli`] writes its own errors, and gives
    /// exit status 1.
    pub fn fail(&self, failure: &Failure, errors: impl Write) -> u8 {
        crate::report(&Error::Start(failure.clone()), self.json, errors)
    }
}
