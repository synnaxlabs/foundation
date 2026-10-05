//! Holds the operation table and handlers, generates the CLI, MCP tools, and docs, and
//! runs each operation on the node that must run it.

use std::ffi::OsString;

use serde_json::{Value, json};

mod error;
mod mcp;
mod operation;
#[cfg(test)]
mod tests;

pub use mcp::mcp;
use operation::{Parsed, TABLE};

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

/// The result of the MCP `tools/list` method: one tool per operation, with its input
/// and output schemas and its `readOnlyHint` and `destructiveHint` annotations.
#[must_use]
pub fn tools() -> Value {
    let inputs = operation::inputs();
    let outputs = operation::outputs();
    let tools: Vec<Value> = TABLE
        .iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "description": spec.summary,
                "inputSchema": inputs[spec.name],
                "outputSchema": outputs[spec.name],
                "annotations": {
                    "readOnlyHint": spec.read_only,
                    "destructiveHint": spec.destructive,
                },
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// The result of the MCP `tools/call` method for the tool `name`, with its
/// `arguments`, or `null` when the call has none. A failed call is a result with
/// `isError` set, and the error's `code`, `message`, and `fix` as its structured
/// content.
#[must_use]
pub fn call(name: &str, arguments: Value) -> Value {
    let (content, failed) = match operation::read(name, arguments) {
        Ok(request) => (request.run().json(), false),
        Err(error) => (error.json(), true),
    };
    json!({
        "content": [{ "type": "text", "text": content.to_string() }],
        "structuredContent": content,
        "isError": failed,
    })
}
