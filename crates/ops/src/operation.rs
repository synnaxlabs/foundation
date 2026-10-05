use std::ffi::OsString;

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The facts that the CLI, the MCP tools, and the docs show for one operation.
pub(crate) struct Spec {
    pub(crate) name: &'static str,
    pub(crate) summary: &'static str,
    pub(crate) read_only: bool,
    pub(crate) destructive: bool,
    /// The JSON Schema of the operation's input.
    pub(crate) schema: fn() -> Value,
}

/// Every operation, in the order the help and the docs list them.
pub(crate) const TABLE: &[Spec] = &[
    Spec {
        name: "version",
        summary: "Print the version of Foundation",
        read_only: true,
        destructive: false,
        schema: schema::<Empty>,
    },
    Spec {
        name: "docs",
        summary: "Print the reference for every operation, as Markdown",
        read_only: true,
        destructive: false,
        schema: schema::<Empty>,
    },
];

fn schema<T: JsonSchema>() -> Value {
    schema_for!(T).to_value()
}

#[derive(Parser)]
#[command(name = "foundation")]
struct Cli {
    /// Print the output or the error as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    request: Request,
}

// Clap and serde name each variant in kebab case; a test holds the names to `TABLE`.
#[derive(Subcommand, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Request {
    Version(Empty),
    Docs(Empty),
}

#[derive(clap::Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Empty {}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
pub(crate) enum Response {
    Version { version: String },
    Docs { markdown: String },
}

/// The command tree, with each operation's summary from `TABLE`.
pub(crate) fn command() -> clap::Command {
    TABLE.iter().fold(Cli::command(), |command, spec| {
        command.mut_subcommand(spec.name, |sub| sub.about(spec.summary))
    })
}

pub(crate) fn parse(args: &[OsString]) -> Result<Request, clap::Error> {
    let matches = command().try_get_matches_from(args)?;
    Ok(Cli::from_arg_matches(&matches)?.request)
}

/// The first line of a clap error, without its `error: ` prefix.
pub(crate) fn message(error: &clap::Error) -> String {
    let text = error.to_string();
    let line = text.lines().next().unwrap_or_default();
    line.strip_prefix("error: ").unwrap_or(line).to_owned()
}

impl Request {
    pub(crate) fn run(self) -> Response {
        match self {
            Self::Version(Empty {}) => Response::Version {
                version: env!("CARGO_PKG_VERSION").to_owned(),
            },
            Self::Docs(Empty {}) => Response::Docs {
                markdown: crate::docs(),
            },
        }
    }
}

impl Response {
    pub(crate) fn json(&self) -> Value {
        serde_json::to_value(self).expect("invariant: a response is plain JSON data")
    }

    pub(crate) fn text(&self) -> String {
        match self {
            Self::Version { version } => format!("{version}\n"),
            Self::Docs { markdown } => markdown.clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn parse(name: &str, json: &str) -> serde_json::Result<Self> {
        let value: Value = serde_json::from_str(json)?;
        let parsed = serde_json::from_value(value)?;
        match (name, &parsed) {
            ("version", Self::Version { .. }) | ("docs", Self::Docs { .. }) => {
                Ok(parsed)
            }
            _ => panic!("`{name}` printed the output of another operation"),
        }
    }
}
