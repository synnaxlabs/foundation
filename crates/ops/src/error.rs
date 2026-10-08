use std::fmt;

use document::diagnostic::Code;
use serde_json::{Value, json};

const ARGUMENT: Code = Code::new("ops.argument");
const UNKNOWN: Code = Code::new("ops.unknown-operation");
const INPUT: Code = Code::new("ops.input");
const OUTPUT: Code = Code::new("ops.output");

/// Why a command line did not run to its end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// An argument is missing, unknown, or has a bad value.
    Argument { message: String },
    /// No operation has this name. `closest` is the nearest name, if one is near.
    Unknown {
        name: String,
        closest: Option<String>,
    },
    /// Standard input could not be read.
    Input { message: String },
    /// Standard output could not be written.
    Output { message: String },
}

impl Error {
    pub(crate) fn code(&self) -> Code {
        match self {
            Self::Argument { .. } => ARGUMENT,
            Self::Unknown { .. } => UNKNOWN,
            Self::Input { .. } => INPUT,
            Self::Output { .. } => OUTPUT,
        }
    }

    pub(crate) fn status(&self) -> u8 {
        match self {
            Self::Argument { .. } | Self::Unknown { .. } => 2,
            Self::Input { .. } | Self::Output { .. } => 1,
        }
    }

    pub(crate) fn fix(&self) -> String {
        match self {
            Self::Argument { .. } => {
                "Match the arguments to the operation in `foundation docs`".to_owned()
            }
            Self::Unknown {
                closest: Some(closest),
                ..
            } => {
                format!("Use `{closest}`, the closest name")
            }
            Self::Unknown { closest: None, .. } => {
                "Use a name from `foundation docs`".to_owned()
            }
            Self::Input { .. } => {
                "Give standard input a source that can be read".to_owned()
            }
            Self::Output { .. } => {
                "Give standard output a destination that can be written".to_owned()
            }
        }
    }

    pub(crate) fn json(&self) -> Value {
        json!({ "code": self.code().as_str(), "message": self.to_string(), "fix": self.fix() })
    }

    /// The error as two lines for a terminal. A backslash or a character that does not
    /// print is written as its Rust escape, so caller text cannot add a line, move the
    /// cursor, or look like other text.
    pub(crate) fn text(&self) -> String {
        let message = escape(&self.to_string());
        format!(
            "error[{}]: {message}\nfix: {}\n",
            self.code(),
            escape(&self.fix())
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Argument { message } => f.write_str(message),
            Self::Unknown { name, .. } => write!(f, "no operation is named `{name}`"),
            Self::Input { message } => {
                write!(f, "standard input could not be read: {message}")
            }
            Self::Output { message } => {
                write!(f, "standard output could not be written: {message}")
            }
        }
    }
}

/// `text` escaped as `char::escape_debug` does, with quotes kept, so it stays one line.
pub(crate) fn escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '"' | '\'' => c.to_string(),
            _ => c.escape_debug().to_string(),
        })
        .collect()
}
