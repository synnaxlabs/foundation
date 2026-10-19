use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use connector::kind::Table;
use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use types::name::Name;

use crate::apply::Applied;
use crate::error::{Among, Error};
use crate::remote::{self, Link};
use crate::scope::Scope;
use crate::serve::Point;
use crate::{Failure, Log, Start, browse, import, plan, status};

/// The data directory of a command that names none.
pub(crate) const DATA: &str = "foundation-data";

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
        summary: "Print the reference for every operation, config block, connector \
                  kind, and secret source, as Markdown",
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "plan",
        summary: "Show the change from config files to the spec of the node of `--data`",
        read_only: false,
        destructive: true,
    },
    Spec {
        name: "apply",
        summary: "Apply a plan file of `foundation plan --out` on the node of `--data`",
        read_only: false,
        destructive: true,
    },
    Spec {
        name: "status",
        summary: "Show the state of each connector and the OS root failures of the node \
                  of `--data`",
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "browse",
        summary: "Show the folders and points of the device of a connector of the node \
                  of `--data`",
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "import",
        summary: "Add the config of points of the device of a connector of the node of \
                  `--data` to the config file of the connector",
        read_only: false,
        destructive: true,
    },
];

#[derive(Parser)]
#[command(
    name = "foundation",
    about = "Run Foundation operations",
    version,
    disable_help_subcommand = true,
    // A bench dependency turns on clap's `wrap_help` in a workspace build, so help
    // never wraps, to be the same in each build.
    term_width = 0
)]
struct Cli {
    /// Print the output or the error as JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

/// The commands. [`command`] gives each operation its summary from `TABLE`.
#[derive(Subcommand)]
enum Command {
    Version(Empty),
    Docs(Empty),
    /// Start a node on a data directory, and run it until Ctrl-C.
    Start(StartArgs),
    Plan(PlanArgs),
    Apply(ApplyArgs),
    Status(NodeArgs),
    Browse(BrowseArgs),
    Import(ImportArgs),
    /// Answer MCP messages on standard input, one JSON-RPC message per line, until it
    /// closes. Each tool call of `plan`, `apply`, `status`, `browse`, and `import`
    /// runs on the node of `--data`, and reads and writes only under the folders of
    /// `--files`, never in the data directory.
    Mcp(McpArgs),
    /// Print this help, or the help of one command.
    Help {
        command: Option<String>,
    },
}

#[derive(clap::Args)]
struct StartArgs {
    /// The data directory of the node. The start makes it when it is not there, but
    /// not its parents.
    #[arg(long, default_value = DATA)]
    data: PathBuf,
    /// The name of the node. The first start on a data directory needs it, and a later
    /// start reads it from there.
    #[arg(long)]
    name: Option<Name>,
    /// The socket address of the node's one port. Port 0 takes a free port. The default
    /// takes no session from another host.
    #[arg(long, default_value = "127.0.0.1:0")]
    listen: SocketAddr,
    /// The least level of an event that the log of the node holds. `debug` and `trace`
    /// write lines for each packet and slow the node.
    #[arg(long, value_enum, default_value_t = Level::Info)]
    log_level: Level,
    /// The form of each line of the log of the node, on standard error. The default is
    /// `json` with `--json`, else `text`.
    #[arg(long, value_enum)]
    log_format: Option<Format>,
}

/// The values of `--log-level`.
#[derive(Clone, Copy, ValueEnum)]
enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl From<Level> for tracing::Level {
    fn from(level: Level) -> Self {
        match level {
            Level::Error => Self::ERROR,
            Level::Warn => Self::WARN,
            Level::Info => Self::INFO,
            Level::Debug => Self::DEBUG,
            Level::Trace => Self::TRACE,
        }
    }
}

/// The values of `--log-format`.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Text,
    Json,
}

#[derive(clap::Args)]
struct PlanArgs {
    #[command(flatten)]
    input: Plan,
    #[command(flatten)]
    node: NodeArgs,
}

#[derive(clap::Args)]
struct ApplyArgs {
    #[command(flatten)]
    input: Apply,
    #[command(flatten)]
    node: NodeArgs,
}

#[derive(clap::Args)]
struct BrowseArgs {
    #[command(flatten)]
    input: Browse,
    #[command(flatten)]
    node: NodeArgs,
}

#[derive(clap::Args)]
struct ImportArgs {
    /// The name of the connector whose points it imports. The node of `--data` runs
    /// it.
    connector: String,
    /// The config files, or directories of config files, that hold the connector. The
    /// import writes the file that holds the connector block.
    #[arg(required = true)]
    files: Vec<PathBuf>,
    /// The folder where the walk starts, as `browse --start` takes it. The default is
    /// the root.
    #[arg(long, value_name = "PATH")]
    start: Option<String>,
    /// A point to import, with its default name: its names from the root, as
    /// `browse` gives its path. Give it once for each point.
    #[arg(long = "point", value_name = "PATH")]
    points: Vec<String>,
    /// A point to import, with the name `<NAME>`. The first `=` splits the two. Give it
    /// once for each point.
    #[arg(long = "name", value_name = "NAME=PATH", value_parser = named)]
    names: Vec<(String, String)>,
    /// Import each point under the start whose path from it matches, as `browse
    /// --match` takes the pattern.
    #[arg(
        long = "match",
        value_name = "PATTERN",
        conflicts_with_all = ["points", "names"]
    )]
    pattern: Option<String>,
    #[command(flatten)]
    node: NodeArgs,
}

/// The name and the path of a `--name` value, split at its first `=`.
fn named(value: &str) -> Result<(String, String), String> {
    value
        .split_once('=')
        .map(|(name, path)| (name.to_owned(), path.to_owned()))
        .ok_or_else(|| format!("`{value}` has no `=` between a name and a path"))
}

#[derive(clap::Args)]
struct McpArgs {
    /// A folder that the tools may read and write, with each folder under it. Give it
    /// once for each folder. The default is the working directory, unless it is `/`
    /// or the home folder.
    #[arg(long, value_name = "DIR")]
    files: Vec<PathBuf>,
    #[command(flatten)]
    node: NodeArgs,
}

/// The arguments of a command that runs on a node.
#[derive(clap::Args)]
struct NodeArgs {
    /// The data directory of the node.
    #[arg(long, default_value = DATA)]
    data: PathBuf,
}

/// The input of each operation, by the name of its tool.
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Request {
    Version(Empty),
    Docs(Empty),
    Plan(Plan),
    Apply(Apply),
    Status(Empty),
    Browse(Browse),
    Import(Import),
}

#[derive(clap::Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Empty {}

#[derive(clap::Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    /// Each config file, or directory of config files, in the order given.
    #[arg(required = true)]
    #[serde(deserialize_with = "some")]
    #[schemars(length(min = 1))]
    files: Vec<PathBuf>,
    /// Write the plan file here, for `foundation apply`. With none, `plan` writes no
    /// file.
    #[arg(long)]
    #[serde(default)]
    out: Option<PathBuf>,
}

#[derive(clap::Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Apply {
    /// The plan file.
    plan: PathBuf,
}

#[derive(clap::Args, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Browse {
    /// The name of the connector whose device it browses. The node of `--data` runs
    /// it.
    pub(crate) connector: String,
    /// The config files, or directories of config files, that hold the connector.
    #[arg(required = true)]
    #[serde(deserialize_with = "some")]
    #[schemars(length(min = 1))]
    pub(crate) files: Vec<PathBuf>,
    /// The folder where the browse starts: its names from the root, joined by `/`,
    /// with a `\` before each `/`, `*`, and `\` in a name. The default is the root.
    #[arg(long, value_name = "PATH")]
    #[serde(default)]
    pub(crate) start: Option<String>,
    /// Give each point under the start whose path from it matches, in place of one
    /// level: `/` splits names, `**` alone matches any number of names, `*` any text
    /// in one name, and `\` takes the next character as it is.
    #[arg(long = "match", value_name = "PATTERN", conflicts_with = "cursor")]
    #[serde(default, rename = "match")]
    pub(crate) pattern: Option<String>,
    /// Give the page of the level from this cursor, which an earlier browse gave.
    #[arg(long)]
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    /// Read the value of each point that it gives.
    #[arg(long)]
    #[serde(default)]
    pub(crate) values: bool,
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Import {
    /// The name of the connector whose points it imports. The node of `--data` runs
    /// it.
    pub(crate) connector: String,
    /// The config files, or directories of config files, that hold the connector. The
    /// import writes the file that holds the connector block.
    #[serde(deserialize_with = "some")]
    #[schemars(length(min = 1))]
    pub(crate) files: Vec<PathBuf>,
    /// The folder where the walk starts, as `browse` takes it. The default is the
    /// root.
    #[serde(default)]
    pub(crate) start: Option<String>,
    /// The points to import, in place of `match`.
    #[serde(default)]
    pub(crate) points: Vec<Point>,
    /// Import each point under the start whose path from it matches, as `browse`
    /// takes the pattern, in place of `points`.
    #[serde(default, rename = "match")]
    pub(crate) pattern: Option<String>,
}

/// A list of at least one path.
fn some<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<PathBuf>, D::Error> {
    let paths = Vec::<PathBuf>::deserialize(deserializer)?;
    if paths.is_empty() {
        return Err(serde::de::Error::invalid_length(0, &"at least one path"));
    }
    Ok(paths)
}

/// The output of each operation. Serde tags it with the operation's name; `json` and
/// `text` leave the tag out.
#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Response {
    Version(Version),
    Docs(Reference),
    Plan(plan::Output),
    Apply(Applied),
    Status(status::Output),
    Browse(browse::Output),
    Import(import::Output),
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub(crate) struct Version {
    /// The version of Foundation.
    pub(crate) version: String,
    /// The 40 or 64 lowercase hex digits of the commit of `HEAD` in the build, or null
    /// with no `git`, outside a git repository, and on a branch with no commit. A build
    /// of a tree with changes gives the commit of `HEAD` too.
    pub(crate) commit: Option<String>,
}

#[derive(Serialize, Deserialize, JsonSchema)]
pub(crate) struct Reference {
    markdown: String,
}

pub(crate) enum Parsed {
    /// Run `request` on the node of `data`, which is `foundation-data` for an
    /// operation that runs on no node.
    Run {
        request: Request,
        data: PathBuf,
    },
    Start(Start),
    Help(String),
    /// Serve MCP, with each tool call on the node of `data`, and its files under the
    /// folders `files`, else the working directory.
    Mcp {
        data: PathBuf,
        files: Vec<PathBuf>,
    },
}

/// The command tree, with each operation's summary from `TABLE`.
pub(crate) fn command() -> clap::Command {
    let root = Cli::command().arg_required_else_help(false);
    TABLE.iter().fold(root, |command, spec| {
        command.mut_subcommand(spec.name, |sub| sub.about(spec.summary))
    })
}

/// Parses `args`. `json` is whether they ask for JSON, as `cli` read it.
pub(crate) fn parse(args: &[OsString], json: bool) -> Result<Parsed, Error> {
    let matches = match command().try_get_matches_from(args) {
        Ok(matches) => matches,
        Err(e) if e.kind() == ErrorKind::DisplayHelp => {
            return Ok(Parsed::Help(e.to_string()));
        }
        Err(e) if e.kind() == ErrorKind::DisplayVersion => {
            return Ok(offline(Request::Version(Empty {})));
        }
        Err(e) => return Err(from_clap(&e)),
    };
    let cli = Cli::from_arg_matches(&matches).map_err(|e| from_clap(&e))?;
    let on = |request, NodeArgs { data }| Parsed::Run { request, data };
    match cli.command {
        Command::Version(empty) => Ok(offline(Request::Version(empty))),
        Command::Docs(empty) => Ok(offline(Request::Docs(empty))),
        Command::Start(StartArgs {
            data,
            name,
            listen,
            log_level,
            log_format,
        }) => Ok(Parsed::Start(Start {
            data,
            json,
            name,
            listen,
            log: Log {
                json: log_format.map_or(json, |format| format == Format::Json),
                level: log_level.into(),
            },
        })),
        Command::Plan(PlanArgs { input, node }) => Ok(on(Request::Plan(input), node)),
        Command::Apply(ApplyArgs { input, node }) => {
            Ok(on(Request::Apply(input), node))
        }
        Command::Status(node) => Ok(on(Request::Status(Empty {}), node)),
        Command::Browse(BrowseArgs { input, node }) => {
            Ok(on(Request::Browse(input), node))
        }
        Command::Import(args) => {
            let (input, node) = args.into_parts();
            Ok(on(Request::Import(input), node))
        }
        Command::Mcp(McpArgs {
            files,
            node: NodeArgs { data },
        }) => Ok(Parsed::Mcp { data, files }),
        Command::Help { command } => help(command.as_deref()),
    }
}

impl ImportArgs {
    /// The input, with each `--point`, then each `--name`, as a point, and the node.
    fn into_parts(self) -> (Import, NodeArgs) {
        let named = self.names.into_iter().map(|(name, path)| Point {
            path,
            name: Some(name),
        });
        let points = self
            .points
            .into_iter()
            .map(|path| Point { path, name: None })
            .chain(named)
            .collect();
        let input = Import {
            connector: self.connector,
            files: self.files,
            start: self.start,
            points,
            pattern: self.pattern,
        };
        (input, self.node)
    }
}

/// The run of `request`, an operation that runs on no node.
fn offline(request: Request) -> Parsed {
    Parsed::Run {
        request,
        data: PathBuf::from(DATA),
    }
}

/// The help that `foundation [name] --help` prints.
fn help(name: Option<&str>) -> Result<Parsed, Error> {
    let mut root = command();
    root.build();
    let command = match name {
        None => &mut root,
        Some(name) => root
            .find_subcommand_mut(name)
            .ok_or_else(|| unknown(name, Among::Commands))?,
    };
    Ok(Parsed::Help(command.render_help().to_string()))
}

/// Reads the request of a `tools/call` of MCP for the tool `name` from its `arguments`.
pub(crate) fn read(
    name: &str,
    arguments: Map<String, Value>,
) -> Result<Request, Error> {
    if !TABLE.iter().any(|spec| spec.name == name) {
        return Err(unknown(name, Among::Tools));
    }
    let tagged = Map::from_iter([(name.to_owned(), Value::Object(arguments))]);
    serde_json::from_value(Value::Object(tagged)).map_err(|e| Error::Argument {
        message: e.to_string(),
    })
}

/// The unknown-operation error for `name`, with the closest operation name.
fn unknown(name: &str, among: Among) -> Error {
    let operations = TABLE
        .iter()
        .fold(clap::Command::new("foundation"), |command, spec| {
            command.subcommand(clap::Command::new(spec.name))
        })
        .disable_help_subcommand(true)
        .disable_help_flag(true);
    let closest = operations
        .try_get_matches_from(["foundation", name])
        .err()
        .and_then(|e| context(&e, ContextKind::SuggestedSubcommand));
    Error::Unknown {
        name: name.to_owned(),
        closest,
        among,
    }
}

fn context(e: &clap::Error, kind: ContextKind) -> Option<String> {
    match e.get(kind) {
        Some(ContextValue::String(text)) => Some(text.clone()),
        Some(ContextValue::Strings(texts)) => texts.first().cloned(),
        _ => None,
    }
}

fn from_clap(e: &clap::Error) -> Error {
    if e.kind() == ErrorKind::InvalidSubcommand {
        return Error::Unknown {
            name: context(e, ContextKind::InvalidSubcommand)
                .expect("invariant: clap names an unknown subcommand"),
            closest: context(e, ContextKind::SuggestedSubcommand),
            among: Among::Commands,
        };
    }
    let what = e
        .kind()
        .as_str()
        .expect("invariant: clap describes each error kind that is not help");
    let mut message = match context(e, ContextKind::InvalidArg) {
        Some(arg) => format!("{what}: `{arg}`"),
        None => what.to_owned(),
    };
    // The error of a value's parser, such as why a name is not one.
    if let Some(source) = std::error::Error::source(e) {
        message = format!("{message}: {source}");
    }
    if let (Some(ContextValue::String(value)), Some(ContextValue::Strings(valid))) = (
        e.get(ContextKind::InvalidValue),
        e.get(ContextKind::ValidValue),
    ) {
        let valid = valid.join(", ");
        message = format!("{message}: `{value}` is not one of {valid}");
    }
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

/// The reference for every operation, config block, kind of `kinds`, and secret
/// source, as Markdown.
pub(crate) fn docs(kinds: &Table) -> String {
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
    format!(
        "# Operations\n{}{}",
        sections.concat(),
        crate::reference::markdown(config::SCHEMA, kinds)
    )
}

impl Request {
    /// Runs the operation, with the files it names read and written through `scope`,
    /// and its request to the node sent through `link`. `docs` states each kind of
    /// `kinds`.
    ///
    /// # Errors
    ///
    /// Each error of [`remote::plan`], [`remote::apply`], [`remote::status`],
    /// [`remote::browse`], and [`remote::import`].
    pub(crate) async fn run<R, C>(
        self,
        scope: &Scope,
        kinds: &Table,
        link: &mut Link<R, C>,
    ) -> Result<Response, Error>
    where
        R: AsyncFnMut() -> Result<C, Failure>,
        C: AsyncFn(&[u8]) -> Result<Vec<u8>, Failure>,
    {
        Ok(match self {
            Self::Version(Empty {}) => Response::Version(Version {
                version: env!("CARGO_PKG_VERSION").to_owned(),
                commit: commit(env!("FOUNDATION_COMMIT")),
            }),
            Self::Docs(Empty {}) => Response::Docs(Reference {
                markdown: docs(kinds),
            }),
            Self::Plan(Plan { files, out }) => {
                Response::Plan(remote::plan(&files, out.as_deref(), scope, link).await?)
            }
            Self::Apply(Apply { plan }) => {
                Response::Apply(remote::apply(&plan, scope, link).await?)
            }
            Self::Status(Empty {}) => Response::Status(remote::status(link).await?),
            Self::Browse(input) => {
                Response::Browse(remote::browse(input, scope, link).await?)
            }
            Self::Import(input) => {
                Response::Import(remote::import(input, scope, link).await?)
            }
        })
    }
}

/// The commit that the build script gives, which is empty when it reads none.
pub(crate) fn commit(built: &str) -> Option<String> {
    Some(built)
        .filter(|commit| !commit.is_empty())
        .map(str::to_owned)
}

impl Response {
    /// The output as JSON, without the operation's name.
    pub(crate) fn json(&self) -> Value {
        match self {
            Self::Version(output) => crate::value(output),
            Self::Docs(output) => crate::value(output),
            Self::Plan(output) => crate::value(output),
            Self::Apply(output) => crate::value(output),
            Self::Status(output) => crate::value(output),
            Self::Browse(output) => crate::value(output),
            Self::Import(output) => crate::value(output),
        }
    }

    pub(crate) fn text(&self) -> String {
        match self {
            Self::Version(output) => {
                let commit = output.commit.as_deref().map_or("unknown", |commit| {
                    commit
                        .get(..12)
                        .expect("invariant: a commit has 40 or 64 hex digits")
                });
                format!("{} ({commit})\n", output.version)
            }
            Self::Docs(output) => output.markdown.clone(),
            Self::Plan(output) => output.text(),
            Self::Apply(output) => output.text(),
            Self::Status(output) => output.text(),
            Self::Browse(output) => output.text(),
            Self::Import(output) => output.text(),
        }
    }
}
