use std::fmt;

use document::diagnostic::Code;
use serde_json::{Value, json};

const ARGUMENT: Code = Code::new("ops.argument");
const UNKNOWN: Code = Code::new("ops.unknown-operation");

/// Why an operation did not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// An argument is missing, unknown, or has a bad value.
    Argument { message: String },
    /// No operation has this name. `closest` is the nearest name, if one is near.
    Unknown {
        name: String,
        closest: Option<String>,
    },
}

impl Error {
    pub(crate) fn code(&self) -> Code {
        match self {
            Self::Argument { .. } => ARGUMENT,
            Self::Unknown { .. } => UNKNOWN,
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
        }
    }

    pub(crate) fn json(&self) -> Value {
        json!({ "code": self.code().as_str(), "message": self.to_string(), "fix": self.fix() })
    }

    pub(crate) fn text(&self) -> String {
        format!("error[{}]: {self}\nfix: {}\n", self.code(), self.fix())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Argument { message } => f.write_str(message),
            Self::Unknown { name, .. } => write!(f, "no operation is named `{name}`"),
        }
    }
}
