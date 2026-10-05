use std::ffi::OsString;

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::Error;

/// The facts that the CLI, the MCP tools, and the docs show for one operation.
pub(crate) struct Spec {
    pub(crate) name: &'static str,
    pub(crate) summary: &'static str,
    pub(crate) read_only: bool,
    pub(crate) destructive: bool,
}

/// Every operation, in the order the help and the docs list them. A test holds the
/// names equal to the variants of `Request` and `Response`.
pub(crate) const TABLE: &[Spec] = &[
    Spec {
        name: "version",
        summary: "Print the version of Foundation",
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "docs",
        summary: "Print the reference for every operation, as Markdown",
        read_only: true,
        destructive: false,
    },
];

#[derive(Parser)]
#[command(name = "foundation")]
struct Cli {
    /// Print the output or the error as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    request: Request,
}

/// The input of each operation. Clap and serde both name a variant in kebab case.
#[derive(Subcommand, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Request {
    Version(Empty),
    Docs(Empty),
}

#[derive(clap::Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Empty {}

/// The output of each operation. Serde tags it with the operation's name; `json` and
/// `text` leave the tag out.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Response {
    Version(Version),
    Docs(Reference),
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub(crate) struct Version {
    version: String,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub(crate) struct Reference {
    markdown: String,
}

pub(crate) enum Parsed {
    Run(Request),
    Help(String),
}

/// The command tree, with each operation's summary from `TABLE`.
pub(crate) fn command() -> clap::Command {
    let root = Cli::command().arg_required_else_help(false);
    TABLE.iter().fold(root, |command, spec| {
        command.mut_subcommand(spec.name, |sub| sub.about(spec.summary))
    })
}

pub(crate) fn parse(args: &[OsString]) -> Result<Parsed, Error> {
    match command().try_get_matches_from(args) {
        Ok(matches) => Cli::from_arg_matches(&matches)
            .map(|cli| Parsed::Run(cli.request))
            .map_err(|e| from_clap(&e)),
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            Ok(Parsed::Help(e.to_string()))
        }
        Err(e) => Err(from_clap(&e)),
    }
}

/// Reads a request for the operation `name` from its JSON `arguments`.
pub(crate) fn read(
    name: &str,
    arguments: Map<String, Value>,
) -> Result<Request, Error> {
    if !TABLE.iter().any(|spec| spec.name == name) {
        return Err(unknown(name));
    }
    let tagged = Map::from_iter([(name.to_owned(), Value::Object(arguments))]);
    serde_json::from_value(Value::Object(tagged)).map_err(|e| Error::Argument {
        message: e.to_string(),
    })
}

/// The unknown-operation error for `name`, with the closest name that the command
/// line would suggest.
fn unknown(name: &str) -> Error {
    match parse(&["foundation".into(), name.into()]) {
        Err(error @ Error::Unknown { .. }) => error,
        _ => Error::Unknown {
            name: name.to_owned(),
            closest: None,
        },
    }
}

fn from_clap(e: &clap::Error) -> Error {
    let context = |kind| match e.get(kind) {
        Some(ContextValue::String(text)) => Some(text.clone()),
        Some(ContextValue::Strings(texts)) => texts.first().cloned(),
        _ => None,
    };
    if e.kind() == ErrorKind::InvalidSubcommand {
        return Error::Unknown {
            name: context(ContextKind::InvalidSubcommand)
                .expect("invariant: clap names an unknown subcommand"),
            closest: context(ContextKind::SuggestedSubcommand),
        };
    }
    let what = e
        .kind()
        .as_str()
        .expect("invariant: clap describes each error kind that is not help");
    let message = match context(ContextKind::InvalidArg) {
        Some(arg) => format!("{what}: `{arg}`"),
        None => what.to_owned(),
    };
    Error::Argument { message }
}

/// The input schema of each operation, by name.
pub(crate) fn inputs() -> Map<String, Value> {
    variants::<Request>()
}

/// The output schema of each operation, by name.
pub(crate) fn outputs() -> Map<String, Value> {
    variants::<Response>()
}

/// The schema of each variant of an externally tagged enum, by its tag.
fn variants<T: JsonSchema>() -> Map<String, Value> {
    let mut settings = SchemaSettings::draft2020_12();
    settings.inline_subschemas = true;
    let root = settings
        .into_generator()
        .into_root_schema_for::<T>()
        .to_value();
    let variants = root["oneOf"]
        .as_array()
        .expect("invariant: an enum of variants");
    variants
        .iter()
        .flat_map(|variant| {
            variant["properties"]
                .as_object()
                .expect("invariant: each variant is tagged by its name")
                .clone()
        })
        .collect()
}

/// The reference for every operation, as Markdown.
pub(crate) fn docs() -> String {
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

impl Request {
    pub(crate) fn run(self) -> Response {
        match self {
            Self::Version(Empty {}) => Response::Version(Version {
                version: env!("CARGO_PKG_VERSION").to_owned(),
            }),
            Self::Docs(Empty {}) => Response::Docs(Reference { markdown: docs() }),
        }
    }
}

impl Response {
    /// The output as JSON, without the operation's name.
    pub(crate) fn json(&self) -> Value {
        let value = match self {
            Self::Version(output) => serde_json::to_value(output),
            Self::Docs(output) => serde_json::to_value(output),
        };
        value.expect("invariant: an output is plain JSON data")
    }

    pub(crate) fn text(&self) -> String {
        match self {
            Self::Version(output) => format!("{}\n", output.version),
            Self::Docs(output) => output.markdown.clone(),
        }
    }
}
