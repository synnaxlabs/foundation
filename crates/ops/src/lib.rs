//! Holds the operation table and handlers, generates the CLI, MCP tools, and docs, and
//! runs each operation on the node that must run it.

use std::ffi::OsString;
use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the table entries of #1744 call it")
)]
mod apply;
#[cfg(test)]
mod common;
mod error;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the table entries of #1744 call it")
)]
mod front_end;
mod mcp;
mod operation;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the table entries of #1744 call it")
)]
mod plan;
#[cfg(test)]
mod tests;

use error::Error;
pub use front_end::FrontEnd;
use operation::Parsed;

/// Runs one command line, such as `["foundation", "version", "--json"]`, and returns
/// the exit status: 0 on success, 1 when a stream fails, and 2 for a bad argument or
/// an unknown operation.
///
/// An operation or help writes its output to `output`, and an error goes to `errors`
/// with its code and fix. With `--json`, each is one line of JSON, and help is
/// `{"help": "<text>"}`. `foundation mcp` answers each line of `input` with
/// at most one line on `output`, flushed, until `input` ends. A line longer than 1 MiB
/// gets an Invalid Request error. A closed `output` ends the run with no error,
/// because its reader has left.
pub fn cli(
    args: impl IntoIterator<Item = OsString>,
    input: impl BufRead,
    mut output: impl Write,
    mut errors: impl Write,
) -> u8 {
    let args: Vec<OsString> = args.into_iter().collect();
    // Read before parsing, so an argument error also honors `--json`.
    let json = args
        .iter()
        .take_while(|arg| *arg != "--")
        .map(|arg| arg.as_encoded_bytes())
        .any(|arg| arg == b"--json" || arg.starts_with(b"--json="));
    let render = |value: Value, text: String| {
        if json { format!("{value}\n") } else { text }
    };
    let done = operation::parse(&args)
        .map_err(Stop::Failed)
        .and_then(|parsed| match parsed {
            Parsed::Run(request) => {
                let response = request.run();
                write(&mut output, &render(response.json(), response.text()))
            }
            Parsed::Help(text) => {
                write(&mut output, &render(json!({ "help": text }), text.clone()))
            }
            Parsed::Mcp => mcp::serve(input, &mut output),
        });
    let error = match done {
        Ok(()) | Err(Stop::Closed) => return 0,
        Err(Stop::Failed(error)) => error,
    };
    match errors.write_all(render(error.json(), error.text()).as_bytes()) {
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
