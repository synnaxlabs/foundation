use std::net::SocketAddr;
use std::path::PathBuf;

use serde_json::Value;
use types::name::Name;

use crate::error;

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
    /// `--listen`: the address of the node's one port, else port 0 of `127.0.0.1`.
    pub listen: SocketAddr,
    /// `--log-format` and `--log-level`.
    pub log: Log,
}

/// How the node writes its log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Log {
    /// `--log-format json`, or `--json` with no `--log-format`: each line is one JSON
    /// object.
    pub json: bool,
    /// `--log-level`: the least level of an event that the log holds.
    pub level: tracing::Level,
}

impl Default for Log {
    /// The log of a `start` with no log flag and no `--json`: text, at INFO.
    fn default() -> Self {
        Self {
            json: false,
            level: tracing::Level::INFO,
        }
    }
}

impl Start {
    /// The line that tells that the node `name` runs in [`Start::data`], as text or
    /// JSON, with its newline. Each gives the data directory lossily, as
    /// `Path::to_string_lossy` does. Text also escapes it as [`crate::fail`] does: the
    /// line stays one line, and a literal `\n` in the path reads apart from a newline.
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
}
