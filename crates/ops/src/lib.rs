//! Holds the operation table and handlers, generates the CLI, MCP tools, and docs, and
//! runs each operation on the node that must run it.

use std::ffi::OsString;

mod error;
mod mcp;
mod operation;
#[cfg(test)]
mod tests;

pub use mcp::mcp;
use operation::Parsed;

/// What one run of the command line writes, and how the process exits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exit {
    /// Text for standard output: the operation's output, as text or as JSON.
    pub stdout: String,
    /// Text for standard error: the error with its code and fix, as text or as JSON.
    pub stderr: String,
    /// The exit status: 0 on success, 2 for a bad argument or an unknown operation.
    pub status: u8,
}

/// Runs one command line, such as `["foundation", "version", "--json"]`. Does no I/O:
/// the caller writes `stdout` and `stderr` and exits with `status`.
pub fn cli(args: impl IntoIterator<Item = OsString>) -> Exit {
    let args: Vec<OsString> = args.into_iter().collect();
    // Read before parsing, so an argument error also honors `--json`.
    let json = args
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--json");
    let (stdout, stderr, status) = match operation::parse(&args) {
        Ok(Parsed::Run(request)) => {
            let output = request.run();
            let text = if json {
                format!("{}\n", output.json())
            } else {
                output.text()
            };
            (text, String::new(), 0)
        }
        Ok(Parsed::Help(text)) => (text, String::new(), 0),
        Err(error) => {
            let text = if json {
                format!("{}\n", error.json())
            } else {
                error.text()
            };
            (String::new(), text, 2)
        }
    };
    Exit {
        stdout,
        stderr,
        status,
    }
}
