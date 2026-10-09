use std::io::Write;
use std::path::PathBuf;

use document::diagnostic::Code;
use serde_json::json;
use types::name::Name;

use crate::error::Error;

/// The arguments of `foundation start`.
#[derive(Clone, Debug, PartialEq, Eq)]
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
    /// Writes that the node `name` runs in [`Start::data`] to `output`, as one line of
    /// text or JSON, and flushes it. A write that fails changes nothing: the node runs
    /// either way. JSON gives the data directory with each byte that is not UTF-8 as
    /// U+FFFD.
    pub fn running(&self, name: &Name, mut output: impl Write) {
        let text = if self.json {
            let data = self.data.to_string_lossy();
            format!("{}\n", json!({ "name": name.as_str(), "data": data }))
        } else {
            let data = self.data.display();
            format!("node {name} runs in {data}. Stop it with Ctrl-C.\n")
        };
        drop(
            output
                .write_all(text.as_bytes())
                .and_then(|()| output.flush()),
        );
    }

    /// Writes `failure` to `errors` as [`crate::cli`] writes its own errors, and gives
    /// exit status 1.
    pub fn fail(&self, failure: &Failure, errors: impl Write) -> u8 {
        crate::report(&Error::Node(failure.clone()), self.json, errors)
    }
}
