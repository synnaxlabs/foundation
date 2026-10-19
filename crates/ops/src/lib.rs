//! Holds the operation table and handlers, generates the CLI, MCP tools, and docs, and
//! runs each operation on the node that must run it.

use std::ffi::OsString;
use std::io::{self, Write};
use std::path::Path;

use connector::kind::Table;
use env::paths::Paths;

use serde::Serialize;
use serde_json::{Value, json};

mod apply;
mod browse;
#[cfg(test)]
mod common;
mod error;
mod front_end;
mod import;
mod mcp;
mod node;
mod operation;
mod plan;
mod reference;
mod remote;
pub mod roots;
mod scope;
mod serve;
mod start;
mod status;
#[cfg(test)]
mod tests;
mod used;

use error::Error;
pub use error::Failure;
pub use front_end::{FrontEnd, FrontEnds};
pub use mcp::{Line, line};
pub use node::{Config, Node};
use operation::Parsed;
use remote::Link;
use scope::Scope;
pub use serve::serve;
pub use start::{Log, Start};

/// What the process does once [`cli`] returns.
#[derive(Debug, PartialEq, Eq)]
pub enum Run {
    /// Exit with this status.
    Exit(u8),
    /// Start a node with these arguments. `cli` wrote nothing for it.
    Start(Start),
}

/// Runs one command line, such as `["foundation", "version", "--json"]`, and returns
/// the exit status: 0 on success, 1 when a stream fails, and 2 for a bad argument or
/// an unknown operation. For `foundation start`, it gives the arguments to start a
/// node with.
///
/// An operation or help writes its output to `output`, and an error goes to `errors`
/// with its code and fix. With `--json`, each is one line of JSON, and help is
/// `{"help": "<text>"}`.
///
/// Only `foundation mcp` calls `input`, once for each line, and answers each line with
/// at most one line on `output`, flushed. A [`Line::Long`] gets an Invalid Request
/// error. It ends with status 0 when `input` gives [`Line::End`], and with status 1
/// and `ops.input` when `input` gives an error. It calls `input` again only after the
/// answer of the line before. A closed `output` ends the run with no error, because
/// its reader has left.
///
/// `plan`, `apply`, `status`, `browse`, and `import` run on the node of `--data`, else
/// of `foundation-data`. Each reads the files it names through `paths`, calls `reach`
/// with that data directory once, and calls what it gives with the body of one request.
/// A [`Failure`] of either is the error of the command, with status 1. A config path of
/// `plan`, `browse`, or `import` that is a directory gives each file in it whose name
/// has an extension of `front_ends` and does not start with `.`, in name order. It
/// gives `ops.no-config` when it has no such file, and `ops.read` for such an entry
/// that is a directory; it reads no file of a directory in it. `foundation mcp` runs
/// each tool call of `plan`, `apply`, `status`, `browse`, and `import` on the node of
/// its own `--data`. It calls `reach` at its first such call, and again only at the
/// call after a [`Failure`] of what `reach` gave. `start` and the other operations call
/// neither.
///
/// `docs` states the body of each kind of `kinds`.
///
/// The tools of `foundation mcp` read and write only under each `--files` folder,
/// else under the working directory, and never in the data directory. With no
/// `--files` and a working directory of `/` or the home folder, it serves nothing,
/// makes nothing, and gives status 2. After it checks its folders, `foundation mcp`
/// makes its data directory when it is not there.
///
/// Blocks its thread at each read or write of a path, and each write of `output` or
/// `errors`, and makes none while a request to the node or a line of `input` waits.
#[expect(
    clippy::too_many_arguments,
    reason = "the parts of the binary, which only `node` builds"
)]
pub async fn cli<C>(
    args: impl IntoIterator<Item = OsString>,
    input: impl AsyncFnMut() -> io::Result<Line>,
    mut output: impl Write,
    errors: impl Write,
    paths: &Paths,
    front_ends: &FrontEnds,
    kinds: &Table,
    mut reach: impl AsyncFnMut(&Path) -> Result<C, Failure>,
) -> Run
where
    C: AsyncFn(&[u8]) -> Result<Vec<u8>, Failure>,
{
    let args: Vec<OsString> = args.into_iter().collect();
    // Read before parsing, so an argument error also honors `--json`.
    let json = json(&args);
    let done = match operation::parse(&args, json) {
        Ok(Parsed::Start(start)) => return Run::Start(start),
        Ok(Parsed::Run { request, data }) => {
            let mut link = Link::new(async || reach(&data).await);
            match request
                .run(&Scope::free(paths, front_ends), kinds, &mut link)
                .await
            {
                Ok(response) => write(
                    &mut output,
                    &render(json, &response.json(), || response.text()),
                ),
                Err(error) => Err(Stop::Failed(error)),
            }
        }
        Ok(Parsed::Help(text)) => write(
            &mut output,
            &render(json, &json!({ "help": text }), || text.clone()),
        ),
        Ok(Parsed::Mcp { data, files }) => {
            match Scope::mcp(paths, front_ends, &files, &data) {
                Ok(scope) => {
                    let mut link = Link::new(async || reach(&data).await);
                    mcp::serve(input, &mut output, &scope, kinds, &mut link).await
                }
                Err(error) => Err(Stop::Failed(error)),
            }
        }
        Err(error) => Err(Stop::Failed(error)),
    };
    match done {
        Ok(()) | Err(Stop::Closed) => Run::Exit(0),
        Err(Stop::Failed(error)) => Run::Exit(report(&error, json, errors)),
    }
}

/// Writes `failure`, why the command line `args` did not run or why its node failed,
/// to `errors` as [`cli`] writes its own errors: as JSON when `args` has `--json`
/// after the program name and before any `--`, else as text. Gives exit status 1.
pub fn fail(args: &[OsString], failure: &Failure, errors: impl Write) -> u8 {
    report(&Error::Node(failure.clone()), json(args), errors)
}

/// Whether `args` asks for JSON: a `--json` flag after the program name and before any
/// `--`.
fn json(args: &[OsString]) -> bool {
    args.iter()
        .skip(1)
        .take_while(|arg| *arg != "--")
        .map(|arg| arg.as_encoded_bytes())
        .any(|arg| arg == b"--json" || arg.starts_with(b"--json="))
}

/// Writes `error` to `errors`, as JSON when `json`, and gives its exit status.
fn report(error: &Error, json: bool, mut errors: impl Write) -> u8 {
    let text = if json {
        format!("{}\n", error.json())
    } else {
        error.text()
    };
    match errors.write_all(text.as_bytes()) {
        // With `errors` closed too, the status is the only report left.
        Ok(()) | Err(_) => error.status(),
    }
}

/// Why a run stopped before its end.
enum Stop {
    /// The reader closed `output`, so the run ends with nothing to report.
    Closed,
    Failed(Error),
}

impl From<Error> for Stop {
    fn from(error: Error) -> Self {
        Self::Failed(error)
    }
}

/// `output` as JSON.
pub(crate) fn value(output: &impl Serialize) -> Value {
    serde_json::to_value(output).expect("invariant: an output is plain JSON data")
}

/// `output` as one line of JSON when `json`, else `text()`.
fn render(
    json: bool,
    output: &impl Serialize,
    text: impl FnOnce() -> String,
) -> String {
    if json {
        format!("{}\n", value(output))
    } else {
        text()
    }
}

/// Writes `text` to `output` and flushes it.
fn write(output: &mut impl Write, text: &str) -> Result<(), Stop> {
    output
        .write_all(text.as_bytes())
        .and_then(|()| output.flush())
        .map_err(|e| match e.kind() {
            io::ErrorKind::BrokenPipe => Stop::Closed,
            _ => Stop::Failed(Error::Output {
                message: e.to_string(),
            }),
        })
}
