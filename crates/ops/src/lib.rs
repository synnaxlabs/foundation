//! Holds the operation table and handlers, generates the CLI, MCP tools, and docs, and
//! runs each operation on the node that must run it.

use std::ffi::OsString;
use std::fmt;

use clap::error::ErrorKind;
use document::diagnostic::Code;
use serde_json::{Value, json};

mod operation;
#[cfg(test)]
mod tests;

use operation::{Request, TABLE};

const ARGUMENT: Code = Code::new("ops.argument");
const UNKNOWN: Code = Code::new("ops.unknown-operation");

/// What one run of the command line writes, and how the process exits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Exit {
    /// Text for standard output: the operation's output, as text or as JSON.
    pub stdout: String,
    /// Text for standard error: the error with its code and fix, as text or as JSON.
    pub stderr: String,
    /// The exit status: 0 on success, 2 for a bad argument.
    pub status: u8,
}

/// Runs one command line, such as `["foundation", "version", "--json"]`. Does no I/O:
/// the caller writes `stdout` and `stderr` and exits with `status`.
pub fn cli(args: impl IntoIterator<Item = OsString>) -> Exit {
    let args: Vec<OsString> = args.into_iter().collect();
    // Read before parsing, so an argument error also honors `--json`.
    let json = args.iter().any(|arg| arg == "--json");
    match operation::parse(&args) {
        Ok(request) => {
            let response = request.run();
            let stdout = if json {
                format!("{}\n", response.json())
            } else {
                response.text()
            };
            Exit {
                stdout,
                stderr: String::new(),
                status: 0,
            }
        }
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            Exit {
                stdout: e.to_string(),
                stderr: String::new(),
                status: 0,
            }
        }
        Err(e) => {
            let error = Error::Argument {
                message: operation::message(&e),
            };
            let stderr = if json {
                let value = json!({
                    "code": error.code().as_str(),
                    "message": error.to_string(),
                    "fix": error.fix(),
                });
                format!("{value}\n")
            } else {
                format!("error[{}]: {error}\nfix: {}\n", error.code(), error.fix())
            };
            Exit {
                stdout: String::new(),
                stderr,
                status: 2,
            }
        }
    }
}

/// The result of the MCP `tools/list` method: one tool per operation, with its input
/// schema and its `readOnlyHint` and `destructiveHint` annotations.
#[must_use]
pub fn tools() -> Value {
    let tools: Vec<Value> = TABLE
        .iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "description": spec.summary,
                "inputSchema": (spec.schema)(),
                "annotations": {
                    "readOnlyHint": spec.read_only,
                    "destructiveHint": spec.destructive,
                },
            })
        })
        .collect();
    json!({ "tools": tools })
}

/// Runs the operation that an MCP `tools/call` names, with its arguments as a JSON
/// object. Returns the operation's output as JSON.
///
/// # Errors
///
/// [`Error::Unknown`] when no operation has the name. [`Error::Argument`] when the
/// arguments do not match the operation's input schema.
pub fn call(name: &str, arguments: Value) -> Result<Value, Error> {
    if !TABLE.iter().any(|spec| spec.name == name) {
        return Err(Error::Unknown {
            name: name.to_owned(),
        });
    }
    let tagged = serde_json::Map::from_iter([(name.to_owned(), arguments)]);
    let request: Request =
        serde_json::from_value(Value::Object(tagged)).map_err(|e| Error::Argument {
            message: e.to_string(),
        })?;
    Ok(request.run().json())
}

/// The reference for every operation, as Markdown. The `docs` operation prints it.
#[must_use]
pub fn docs() -> String {
    let yes = |flag: bool| if flag { "yes" } else { "no" };
    let sections: Vec<String> = TABLE
        .iter()
        .map(|spec| {
            format!(
                "\n## `{}`\n\n{}\n\n- Read-only: {}\n- Destructive: {}\n",
                spec.name,
                spec.summary,
                yes(spec.read_only),
                yes(spec.destructive),
            )
        })
        .collect();
    format!("# Operations\n{}", sections.concat())
}

/// Why an operation did not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// An argument is missing, unknown, or has a bad value.
    Argument {
        /// What is wrong, as a clause with no final period.
        message: String,
    },
    /// No operation has this name.
    Unknown {
        /// The name that was asked for.
        name: String,
    },
}

impl Error {
    /// The stable code, such as `ops.argument`.
    #[must_use]
    pub fn code(&self) -> Code {
        match self {
            Self::Argument { .. } => ARGUMENT,
            Self::Unknown { .. } => UNKNOWN,
        }
    }

    /// How to fix the problem, as a sentence with no final period.
    #[must_use]
    pub fn fix(&self) -> String {
        match self {
            Self::Argument { .. } => {
                "Match the arguments to the operation in `foundation docs`"
            }
            Self::Unknown { .. } => "Use a name from `foundation docs`",
        }
        .to_owned()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Argument { message } => f.write_str(message),
            Self::Unknown { name } => write!(f, "no operation is named `{name}`"),
        }
    }
}

impl std::error::Error for Error {}
