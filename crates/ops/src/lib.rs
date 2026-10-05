//! Holds the operation table and handlers, generates the CLI, MCP tools, and docs, and
//! runs each operation on the node that must run it.

use std::ffi::OsString;
use std::io::{self, BufRead, Write};

mod error;
mod mcp;
mod operation;
#[cfg(test)]
mod tests;

use error::Error;
use operation::Parsed;

/// Runs one command line, such as `["foundation", "version", "--json"]`, and returns
/// the exit status: 0 on success, 1 when a stream fails, and 2 for a bad argument or
/// an unknown operation.
///
/// An operation writes its output to `output`. An error goes to `errors` with its code
/// and fix, as JSON with `--json`. `foundation mcp` answers each line of `input` with
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
        .any(|arg| arg == "--json");
    let done = operation::parse(&args)
        .map_err(Stop::Failed)
        .and_then(|parsed| match parsed {
            Parsed::Run(request) => {
                let response = request.run();
                let text = if json {
                    format!("{}\n", response.json())
                } else {
                    response.text()
                };
                write(&mut output, &text)
            }
            Parsed::Help(text) => write(&mut output, &text),
            Parsed::Mcp => mcp::serve(input, &mut output),
        });
    let error = match done {
        Ok(()) | Err(Stop::Closed) => return 0,
        Err(Stop::Failed(error)) => error,
    };
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
