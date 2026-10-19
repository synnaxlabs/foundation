use std::cell::Cell;
use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use connector::browse::Malformed;
use document::Source;
use serde_json::{Map, Value, json};

use crate::error::{
    Action, Among, Bound, Error, On, Problem, Sources, Unread, Unwritten,
};
use crate::operation::{self, Response, TABLE};
use crate::{FrontEnd, FrontEnds};
use crate::{front_end, plan};

/// The fix of `ops.argument`.
const HELP: &str =
    "Match the arguments to `foundation --help` or `foundation <operation> --help`";

#[derive(Debug, PartialEq, Eq)]
struct Exit {
    stdout: String,
    stderr: String,
    status: u8,
}

/// What [`crate::cli`] gives for a command that reaches no node, with the paths of a
/// host that holds no file. Such a command does not wait, so one poll ends it.
fn offline(
    args: impl IntoIterator<Item = OsString>,
    mut input: impl io::BufRead,
    output: impl io::Write,
    errors: impl io::Write,
) -> crate::Run {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = sim.node(sim::node::Config::default());
    host.make_dir(Path::new("/work"));
    host.set_working_dir(Path::new("/work"));
    let paths = host.paths();
    let request = async |_: &[u8]| -> Result<Vec<u8>, crate::Failure> {
        panic!("a command sent a request")
    };
    let reached = Cell::new(0);
    let reach = async |_: &Path| {
        reached.set(reached.get() + 1);
        Ok(request)
    };
    let input = async || crate::line(&mut input);
    let front_ends = crate::common::front_ends();
    let kinds = crate::common::create_kinds();
    let cli = crate::cli(
        args,
        input,
        output,
        errors,
        &paths,
        &front_ends,
        &kinds,
        reach,
    );
    let Poll::Ready(run) = pin!(cli).poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("a command waited");
    };
    assert_eq!(reached.get(), 0, "a command reached its node");
    run
}

fn run(args: &[&str], input: impl io::BufRead) -> Exit {
    let args = ["foundation"].iter().chain(args).map(OsString::from);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let crate::Run::Exit(status) = offline(args, input, &mut stdout, &mut stderr)
    else {
        panic!("a command that is not `start` exits");
    };
    Exit {
        stdout: String::from_utf8(stdout).expect("UTF-8"),
        stderr: String::from_utf8(stderr).expect("UTF-8"),
        status,
    }
}

fn cli(args: &[&str]) -> Exit {
    run(args, io::empty())
}

fn failed(stderr: &str) -> Exit {
    Exit {
        stdout: String::new(),
        stderr: stderr.to_owned(),
        status: 2,
    }
}

/// The reply of [`crate::mcp::respond`] to `message`, with the paths of a host that
/// holds no file and a link that reaches no node. It must not wait.
fn respond(message: &str) -> Option<String> {
    let mut sim = sim::Sim::new(sim::Config::default());
    let paths = sim.node(sim::node::Config::default()).paths();
    let request = async |_: &[u8]| -> Result<Vec<u8>, crate::Failure> {
        panic!("a message sent a request")
    };
    let reached = Cell::new(0);
    let mut link = crate::remote::Link::new(async || {
        reached.set(reached.get() + 1);
        Ok(request)
    });
    let scope = crate::scope::Scope::free(&paths, &crate::common::front_ends());
    let kinds = crate::common::create_kinds();
    let respond = crate::mcp::respond(message, &scope, &kinds, &mut link);
    let Poll::Ready(reply) =
        pin!(respond).poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("a message waited");
    };
    assert_eq!(reached.get(), 0, "a message reached a node");
    reply
}

fn ask(message: &Value) -> Value {
    let reply = respond(&message.to_string()).expect("a reply");
    assert!(!reply.contains('\n'), "one line: {reply}");
    serde_json::from_str(&reply).expect("json")
}

fn tools() -> Value {
    ask(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }))["result"].take()
}

fn call(params: &Value) -> Value {
    ask(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params }))
}

fn failed_call(message: &str, data: &Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "error": { "code": -32602, "message": message, "data": data },
    })
}

fn names(map: &Map<String, Value>) -> BTreeSet<&str> {
    map.keys().map(String::as_str).collect()
}

#[test]
fn the_table_names_each_input_and_output_once() {
    let names_in_table: BTreeSet<_> = TABLE.iter().map(|spec| spec.name).collect();
    assert_eq!(names_in_table.len(), TABLE.len());
    assert_eq!(names(&operation::inputs()), names_in_table);
    assert_eq!(names(&operation::outputs()), names_in_table);
}

#[test]
fn each_operation_appears_once_in_the_cli_the_tools_and_the_docs() {
    let command = operation::command();
    let tools = tools();
    let tools = tools["tools"].as_array().expect("tools is a list");
    let docs = operation::docs(&crate::common::create_kinds());
    // Three more for `start`, `mcp`, and `help`, which are not operations.
    assert_eq!(command.get_subcommands().count(), TABLE.len() + 3);
    assert_eq!(tools.len(), TABLE.len());
    let inputs = operation::inputs();
    let outputs = operation::outputs();
    for spec in TABLE {
        let sub = command
            .find_subcommand(spec.name)
            .unwrap_or_else(|| panic!("no subcommand {}", spec.name));
        assert_eq!(
            sub.get_about().map(ToString::to_string).as_deref(),
            Some(spec.summary)
        );
        let tool: Vec<_> = tools.iter().filter(|t| t["name"] == spec.name).collect();
        assert_eq!(tool.len(), 1, "{}", spec.name);
        assert_eq!(tool[0]["description"], spec.summary);
        assert_eq!(tool[0]["annotations"]["readOnlyHint"], spec.read_only);
        assert_eq!(tool[0]["annotations"]["destructiveHint"], spec.destructive);
        assert_eq!(tool[0]["inputSchema"], inputs[spec.name]);
        assert_eq!(tool[0]["outputSchema"], outputs[spec.name]);
        let yes = |flag| if flag { "yes" } else { "no" };
        let section = format!(
            "## `{}`\n\n{}\n\n- Read-only: {}\n- Destructive: {}\n",
            spec.name,
            spec.summary,
            yes(spec.read_only),
            yes(spec.destructive),
        );
        assert_eq!(docs.matches(&section).count(), 1, "{section}");
        assert_eq!(docs.matches(&format!("## `{}`", spec.name)).count(), 1);
    }
}

#[test]
fn each_schema_is_a_closed_object() {
    for (name, schema) in operation::inputs().iter().chain(&operation::outputs()) {
        assert_eq!(schema["type"], "object", "{name}");
    }
    for name in operation::inputs().keys() {
        assert_eq!(
            operation::inputs()[name]["additionalProperties"],
            false,
            "{name}"
        );
    }
}

/// `remote::tests` holds the case of each operation that runs on a node.
#[test]
fn json_output_parses_back_to_the_typed_output() {
    for name in ["version", "docs"] {
        let exit = cli(&[name, "--json"]);
        assert_eq!((exit.status, exit.stderr.as_str()), (0, ""), "{name}");
        let output: Value = serde_json::from_str(&exit.stdout).expect("json");
        let response: Response =
            serde_json::from_value(json!({ name: output.clone() })).expect(name);
        assert_eq!(response.json(), output);
        let call = call(&json!({ "name": name, "arguments": {} }));
        assert_eq!(
            call["result"],
            json!({
                "content": [{ "type": "text", "text": output.to_string() }],
                "structuredContent": output,
            })
        );
    }
}

/// The commit of `HEAD` that `git` reads here, or `None` when it fails.
fn head() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    let text = String::from_utf8(output.stdout).expect("UTF-8");
    output.status.success().then(|| text.trim().to_owned())
}

#[test]
fn version_prints_the_crate_version_and_the_commit() {
    let commit = head();
    let json = cli(&["version", "--json"]);
    let output: Value = serde_json::from_str(&json.stdout).expect("json");
    assert_eq!(
        output,
        json!({ "version": env!("CARGO_PKG_VERSION"), "commit": commit })
    );
    let commit = commit.as_deref().map_or("unknown", |commit| &commit[..12]);
    let line = format!("{} ({commit})\n", env!("CARGO_PKG_VERSION"));
    assert_eq!((cli(&["version"]).stdout, json.status), (line, 0));
}

#[test]
fn version_gives_no_commit_when_the_build_reads_none() {
    let built = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(
        (
            crate::operation::commit(""),
            crate::operation::commit(built)
        ),
        (None, Some(built.to_owned()))
    );
}

#[test]
fn the_version_flag_runs_the_operation_version() {
    for (flag, operation) in [
        (&["--version"][..], &["version"][..]),
        (&["--version", "--json"], &["version", "--json"]),
        (&["-V"], &["version"]),
    ] {
        assert_eq!(cli(flag), cli(operation), "{flag:?}");
    }
}

#[test]
fn version_writes_twelve_digits_of_the_commit_or_unknown() {
    let commit = "8e29d7d536cd11b8243439baa91128f6ed9ee497";
    let output = |commit: Option<&str>| {
        Response::Version(operation::Version {
            version: "0.0.0".to_owned(),
            commit: commit.map(str::to_owned),
        })
    };
    let built = output(Some(commit));
    assert_eq!(
        (built.text(), built.json()),
        (
            "0.0.0 (8e29d7d536cd)\n".to_owned(),
            json!({ "version": "0.0.0", "commit": commit })
        )
    );
    let unknown = output(None);
    assert_eq!(
        (unknown.text(), unknown.json()),
        (
            "0.0.0 (unknown)\n".to_owned(),
            json!({ "version": "0.0.0", "commit": null })
        )
    );
}

#[test]
fn docs_prints_the_reference() {
    let exit = cli(&["docs"]);
    assert_eq!(
        exit,
        Exit {
            stdout: operation::docs(&crate::common::create_kinds()),
            stderr: String::new(),
            status: 0
        }
    );
}

#[test]
fn help_goes_to_standard_output() {
    let exit = cli(&["--help"]);
    assert_eq!((exit.status, exit.stderr.as_str()), (0, ""));
    for spec in TABLE {
        assert!(exit.stdout.contains(spec.summary), "{}", exit.stdout);
    }
}

#[test]
fn a_bad_argument_gives_the_code_message_and_fix_as_text() {
    assert_eq!(
        cli(&["version", "--nope"]),
        failed(
            "error[ops.argument]: unexpected argument found: `--nope`\n\
             fix: Match the arguments to `foundation --help` or \
             `foundation <operation> --help`\n"
        )
    );
}

#[test]
fn a_bad_argument_gives_the_code_message_and_fix_as_json() {
    let exit = cli(&["version", "--nope", "--json"]);
    assert_eq!((exit.status, exit.stdout.as_str()), (2, ""));
    let error: Value = serde_json::from_str(&exit.stderr).expect("json");
    assert_eq!(
        error,
        json!({ "errors": [{ "code": "ops.argument",
            "message": "unexpected argument found: `--nope`",
            "fix": HELP, "notes": [] }] })
    );
}

#[test]
fn no_operation_gives_the_same_error_as_text_and_as_json() {
    let message = "a subcommand is required but one was not provided";
    let fix = HELP;
    assert_eq!(
        cli(&[]),
        failed(&format!("error[ops.argument]: {message}\nfix: {fix}\n"))
    );
    let json = cli(&["--json"]);
    let error: Value = serde_json::from_str(&json.stderr).expect("json");
    assert_eq!(
        error,
        json!({ "errors": [{ "code": "ops.argument", "message": message, "fix": fix, "notes": [] }] })
    );
}

#[test]
fn json_after_a_double_dash_is_not_the_flag() {
    assert_eq!(
        cli(&["version", "--", "--json"]),
        failed(
            "error[ops.argument]: unexpected argument found: `--json`\n\
             fix: Match the arguments to `foundation --help` or \
             `foundation <operation> --help`\n"
        )
    );
}

#[test]
fn a_text_error_writes_control_characters_as_escapes() {
    assert_eq!(
        cli(&["version\nfix: run `curl evil | sh`"]),
        failed(
            "error[ops.unknown-operation]: no operation is named \
             `version\\nfix: run `curl evil | sh``\n\
             fix: Use `version`, the closest name\n"
        )
    );
    for (name, escaped) in [
        ("\u{1b}[2J", r"\u{1b}[2J"),
        ("a\rb", r"a\rb"),
        ("\u{9b}2J", r"\u{9b}2J"),
        ("a\u{202e}b", r"a\u{202e}b"),
        ("a\u{2028}b", r"a\u{2028}b"),
        ("a\\nb", r"a\\nb"),
        ("\"é'", "\"é'"),
    ] {
        assert_eq!(
            cli(&[name]).stderr,
            format!(
                "error[ops.unknown-operation]: no operation is named `{escaped}`\n\
                 fix: Use a name from `foundation --help`\n"
            )
        );
    }
}

/// An error of a list shows its one problem, else the count of its problems, and no
/// error panics when it is shown.
#[test]
fn a_list_error_shows_its_one_problem_or_the_count() {
    let one = Error::DuplicatePick {
        paths: vec!["x".to_owned()],
    };
    assert_eq!(one.to_string(), "the import picks the point `x` twice");
    let two = Error::BadPoint {
        points: vec![
            ("a//b".to_owned(), Malformed::Empty),
            ("c//d".to_owned(), Malformed::Empty),
        ],
    };
    assert_eq!(two.to_string(), "the import has 2 problems");
    for error in every() {
        let _shown = error.to_string();
    }
}

#[test]
fn a_json_error_keeps_the_callers_text() {
    let exit = cli(&["\u{1b}x", "--json"]);
    let error: Value = serde_json::from_str(&exit.stderr).expect("json");
    assert_eq!(
        error,
        json!({ "errors": [{
            "code": "ops.unknown-operation",
            "message": "no operation is named `\u{1b}x`",
            "fix": "Use a name from `foundation --help`",
            "notes": [],
        }] })
    );
}

#[test]
fn help_with_the_json_flag_is_json() {
    let text = cli(&["--help"]).stdout;
    for args in [
        ["--json", "help"],
        ["help", "--json"],
        ["--json", "--help"],
        ["--help", "--json"],
    ] {
        let exit = cli(&args);
        assert_eq!((exit.status, exit.stderr.as_str()), (0, ""));
        let help: Value = serde_json::from_str(&exit.stdout).expect("json");
        assert_eq!(help, json!({ "help": text }), "{args:?}");
    }
}

#[test]
fn help_for_a_command_matches_its_help_flag() {
    for name in ["version", "mcp"] {
        let exit = cli(&["help", name]);
        assert_eq!(exit, cli(&[name, "--help"]), "{name}");
        assert!(
            exit.stdout.contains(&format!("Usage: foundation {name}")),
            "{}",
            exit.stdout
        );
    }
    let exit = cli(&["help", "version", "--json"]);
    let help: Value = serde_json::from_str(&exit.stdout).expect("json");
    assert_eq!(help, json!({ "help": cli(&["version", "--help"]).stdout }));
}

#[test]
fn help_for_a_name_that_is_not_a_command_is_unknown_and_help_of_help_is_help() {
    assert_eq!(
        cli(&["help", "versoin"]),
        failed(
            "error[ops.unknown-operation]: no operation is named `versoin`\n\
             fix: Use `version`, the closest name\n"
        )
    );
    assert_eq!(
        cli(&["help", "zzz"]),
        failed(
            "error[ops.unknown-operation]: no operation is named `zzz`\n\
             fix: Use a name from `foundation --help`\n"
        )
    );
    let help = cli(&["help", "help"]);
    assert_eq!(help.status, 0);
    assert_eq!(help, cli(&["help", "--help"]));
}

#[test]
fn a_json_flag_with_a_value_gives_its_error_as_json() {
    use std::os::unix::ffi::OsStringExt;
    for value in [
        OsString::from("--json=true"),
        OsString::from_vec(b"--json=\xff".to_vec()),
    ] {
        let args = [
            OsString::from("foundation"),
            "version".into(),
            value.clone(),
        ];
        let mut stderr = Vec::new();
        let status = offline(args, io::empty(), io::sink(), &mut stderr);
        let error: Value = serde_json::from_slice(&stderr).expect("json");
        assert_eq!(
            (status, error),
            (
                crate::Run::Exit(2),
                json!({ "errors": [{ "code": "ops.argument",
                    "message": "unexpected value for an argument found: `--json`",
                    "fix": HELP, "notes": [] }] })
            ),
            "{value:?}"
        );
    }
    assert_eq!(
        cli(&["version", "--jsonx"]),
        failed(
            "error[ops.argument]: unexpected argument found: `--jsonx`\n\
             fix: Match the arguments to `foundation --help` or \
             `foundation <operation> --help`\n"
        )
    );
}

#[test]
fn an_unknown_operation_suggests_the_closest_name() {
    assert_eq!(
        cli(&["versoin"]),
        failed(
            "error[ops.unknown-operation]: no operation is named `versoin`\n\
             fix: Use `version`, the closest name\n"
        )
    );
    assert_eq!(
        call(&json!({ "name": "versoin" })),
        failed_call(
            "no operation is named `versoin`",
            &json!({ "errors": [{ "code": "ops.unknown-operation",
                "message": "no operation is named `versoin`",
                "fix": "Use `version`, the closest name", "notes": [] }] })
        )
    );
}

#[test]
fn an_unknown_operation_far_from_every_name_points_to_the_list_of_its_carrier() {
    let expected = |fix| {
        json!({ "errors": [{ "code": "ops.unknown-operation",
            "message": "no operation is named `zzz`", "fix": fix, "notes": [] }] })
    };
    assert_eq!(
        call(&json!({ "name": "zzz" })),
        failed_call(
            "no operation is named `zzz`",
            &expected("Use a name from `tools/list`")
        )
    );
    let exit = cli(&["zzz", "--json"]);
    assert_eq!((exit.status, exit.stdout.as_str()), (2, ""));
    assert_eq!(
        serde_json::from_str::<Value>(&exit.stderr).expect("json"),
        expected("Use a name from `foundation --help`")
    );
}

#[test]
fn a_call_with_a_bad_argument_names_it() {
    let message = "unknown field `nope`, there are no fields";
    assert_eq!(
        call(&json!({ "name": "version", "arguments": { "nope": 1 } })),
        failed_call(
            message,
            &json!({ "errors": [{ "code": "ops.argument",
                "message": message,
                "fix": HELP, "notes": [] }] })
        )
    );
}

#[test]
fn a_call_with_no_arguments_runs() {
    assert_eq!(
        call(&json!({ "name": "version" }))["result"]["structuredContent"]["version"],
        env!("CARGO_PKG_VERSION")
    );
}

/// One error of each variant but `Config`, `Node`, and `Remote`.
#[expect(clippy::too_many_lines, reason = "one value of each variant")]
fn every() -> Vec<Error> {
    let pointer = spec::Pointer {
        version: 0,
        root: spec::tree::empty(),
    };
    vec![
        Error::Argument {
            message: String::new(),
        },
        Error::Unknown {
            name: String::new(),
            closest: None,
            among: Among::Commands,
        },
        Error::Input {
            message: String::new(),
        },
        Error::Output {
            message: String::new(),
        },
        Error::Plan(config::plan::Error::Malformed { at: 0 }),
        Error::Behind(Box::new(mesh::used::Behind {
            pointer,
            cause: mesh::used::Cause::Problems(Vec::new()),
        })),
        Error::Stale {
            base: pointer,
            pointer,
        },
        Error::Apply(mesh::Error::NoVote),
        Error::Stopped(mesh::Stopped::Dropped),
        Error::Request {
            message: String::new(),
        },
        Error::Denied {
            subject: "bob".parse().expect("a name"),
            action: Action::Plan,
            on: On::Spec,
        },
        Error::LargeReply,
        Error::LargeBrowse,
        Error::LargeStatus,
        Error::ChannelRead {
            connector: "site.a".parse().expect("a name"),
            message: String::new(),
        },
        Error::Read {
            path: PathBuf::new(),
            cause: Unread::NotFound,
        },
        Error::NoConfig {
            path: PathBuf::from("d"),
            extensions: "`.hcl`".to_owned(),
        },
        Error::Write {
            path: PathBuf::new(),
            cause: Unwritten::NotFound,
        },
        Error::LargeRequest,
        Error::Reply {
            message: String::new(),
        },
        Error::Outside {
            path: PathBuf::new(),
            limits: crate::scope::Limits {
                folders: vec![PathBuf::from("/work")],
                data: PathBuf::from("/work/foundation-data"),
                working: PathBuf::from("/work"),
            },
        },
        Error::Hidden {
            path: PathBuf::new(),
            data: PathBuf::from("/work/foundation-data"),
        },
        Error::NoData {
            path: PathBuf::new(),
            data: PathBuf::from("/work/foundation-data"),
        },
        Error::FolderChanged {
            path: PathBuf::new(),
            folders: vec![PathBuf::from("/work")],
        },
        Error::PlanName {
            path: PathBuf::new(),
            resolved: PathBuf::new(),
        },
        Error::WideFolder {
            working: PathBuf::new(),
        },
        Error::Folder {
            path: PathBuf::new(),
            cause: crate::error::Unusable::File,
        },
        Error::Data {
            path: PathBuf::from("/work/foundation-data"),
            error: env::paths::Error::NotFound,
        },
        Error::browse(
            &"plant".parse().expect("a name"),
            &"opcua".parse().expect("a name"),
            connector::kind::Error::Device("lost".into()),
            &Sources::default(),
        ),
        Error::BrowseNode {
            connector: "plant".parse().expect("a name"),
            node: "edge".parse().expect("a name"),
            this: "core".parse().expect("a name"),
        },
        Error::BadStart {
            text: String::new(),
            cause: Malformed::Empty,
        },
        Error::BadPattern {
            text: String::new(),
            cause: Malformed::Escape,
        },
        Error::NoPick,
        Error::PickLimit { picked: 501 },
        Error::MatchLimit,
        Error::BadPoint {
            points: vec![(String::new(), Malformed::Empty)],
        },
        Error::DuplicatePick {
            paths: vec!["Line/Pump".to_owned()],
        },
        Error::BadName {
            names: vec![(String::new(), String::new(), types::name::Error::Empty)],
        },
        Error::UnreadablePoint {
            points: vec![("Line/Pump".to_owned(), "no access".to_owned())],
        },
        Error::PickBound(Bound::Budget),
        Error::MatchBound(Bound::Budget),
        Error::LargeImport,
        Error::FileChanged {
            path: PathBuf::new(),
        },
    ]
}

/// Matches each variant, so a new variant fails to compile until it joins [`every`]
/// and the golden file.
fn listed(error: &Error) {
    // `Config` holds the codes of the front ends, of `config`, and the ones below.
    // `Node` holds the codes of `node`, and `Remote` the codes of a reply.
    match error {
        Error::Argument { .. }
        | Error::Unknown { .. }
        | Error::Input { .. }
        | Error::Output { .. }
        | Error::Plan(_)
        | Error::Behind(_)
        | Error::Stale { .. }
        | Error::Apply(_)
        | Error::Stopped(_)
        | Error::Request { .. }
        | Error::Denied { .. }
        | Error::LargeReply
        | Error::LargeBrowse
        | Error::LargeStatus
        | Error::ChannelRead { .. }
        | Error::Read { .. }
        | Error::NoConfig { .. }
        | Error::Write { .. }
        | Error::LargeRequest
        | Error::Reply { .. }
        | Error::Outside { .. }
        | Error::Hidden { .. }
        | Error::NoData { .. }
        | Error::FolderChanged { .. }
        | Error::PlanName { .. }
        | Error::WideFolder { .. }
        | Error::Folder { .. }
        | Error::Data { .. }
        | Error::Browse { .. }
        | Error::BrowseNode { .. }
        | Error::BadStart { .. }
        | Error::BadPattern { .. }
        | Error::NoPick
        | Error::PickLimit { .. }
        | Error::MatchLimit
        | Error::BadPoint { .. }
        | Error::DuplicatePick { .. }
        | Error::BadName { .. }
        | Error::UnreadablePoint { .. }
        | Error::PickBound(_)
        | Error::MatchBound(_)
        | Error::LargeImport
        | Error::FileChanged { .. }
        | Error::Config(_)
        | Error::Node(_)
        | Error::Remote { .. } => {}
    }
}

#[test]
fn error_codes_and_fixes_match_the_golden_file() {
    let every = every();
    every.iter().for_each(listed);
    let mut lines: Vec<_> = every
        .iter()
        .flat_map(|error| error.problems().into_owned())
        .chain([Problem::more(0)])
        .map(|problem| format!("{}\t{}\n", problem.code, problem.fix))
        .collect();
    let front_ends = FrontEnds::new(
        "hcl",
        FrontEnd {
            read: |_, _| Err(Vec::new()),
            update: |_, _, _| Err(Vec::new()),
        },
    );
    let unknown = front_end::unknown(Source(0), &front_ends);
    let not_utf8 = front_end::not_utf8(Path::new("plant.yaml"));
    let unresolved = plan::unresolved(None, String::new());
    for diagnostic in [unknown, not_utf8, unresolved] {
        lines.push(format!("{}\t{}\n", diagnostic.code, diagnostic.fix));
    }
    let codes: BTreeSet<_> = lines.iter().map(|line| line.split('\t').next()).collect();
    assert_eq!(codes.len(), lines.len(), "a code has one cause");
    assert_eq!(lines.concat(), include_str!("codes.golden"));
}

mod start {
    use std::ffi::OsString;
    use std::io::{self, Write};
    use std::net::{Ipv4Addr, SocketAddr};
    use std::path::PathBuf;

    use document::diagnostic::Code;
    use serde_json::json;

    use super::{Exit, call, cli, failed, offline};
    use crate::{Failure, Log, Run, Start};

    fn start(args: &[&str]) -> Run {
        let args = ["foundation", "start"]
            .iter()
            .chain(args)
            .map(OsString::from);
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let run = offline(args, io::empty(), &mut stdout, &mut stderr);
        assert_eq!((stdout, stderr), (Vec::new(), Vec::new()), "writes nothing");
        run
    }

    fn local() -> SocketAddr {
        SocketAddr::from((Ipv4Addr::LOCALHOST, 0))
    }

    fn edge() -> types::name::Name {
        "edge".parse().unwrap()
    }

    fn busy() -> Failure {
        Failure {
            code: Code::new("node.busy"),
            message: "another node runs in foundation-data".to_owned(),
            fix: "Stop that node, or give another data directory with `--data`"
                .to_owned(),
        }
    }

    #[test]
    fn start_with_no_flag_uses_foundation_data_and_no_name() {
        assert_eq!(
            start(&[]),
            Run::Start(Start {
                data: PathBuf::from("foundation-data"),
                json: false,
                name: None,
                listen: local(),
                log: Log::default(),
            })
        );
    }

    #[test]
    fn start_gives_its_flags() {
        assert_eq!(
            start(&[
                "--data",
                "/srv/a",
                "--name",
                "site_a.edge",
                "--listen",
                "[::1]:7000",
                "--log-level",
                "debug",
                "--log-format",
                "json",
            ]),
            Run::Start(Start {
                data: PathBuf::from("/srv/a"),
                json: false,
                name: Some("site_a.edge".parse().unwrap()),
                listen: "[::1]:7000".parse().unwrap(),
                log: Log {
                    json: true,
                    level: tracing::Level::DEBUG,
                },
            })
        );
    }

    #[test]
    fn the_default_log_is_text_at_info() {
        let text = Log {
            json: false,
            level: tracing::Level::INFO,
        };
        assert_eq!(Log::default(), text);
    }

    /// The log of `start` with `args`.
    fn log(args: &[&str]) -> Log {
        match start(args) {
            Run::Start(start) => start.log,
            Run::Exit(status) => panic!("start exited with {status}"),
        }
    }

    #[test]
    fn log_level_gives_each_of_its_five_levels() {
        for (arg, level) in [
            ("error", tracing::Level::ERROR),
            ("warn", tracing::Level::WARN),
            ("info", tracing::Level::INFO),
            ("debug", tracing::Level::DEBUG),
            ("trace", tracing::Level::TRACE),
        ] {
            assert_eq!(log(&["--log-level", arg]).level, level, "{arg}");
        }
    }

    #[test]
    fn the_log_format_follows_json_unless_given() {
        let format = |args: &[&str]| log(args).json;
        assert!(!format(&[]));
        assert!(format(&["--json"]));
        assert!(!format(&["--json", "--log-format", "text"]));
        assert!(format(&["--log-format", "json"]));
        assert!(!format(&["--log-format", "text"]));
    }

    #[test]
    fn a_log_level_out_of_the_five_is_an_argument_error_that_lists_them() {
        for value in ["INFO", "off"] {
            assert_eq!(
                cli(&["start", "--log-level", value]),
                failed(&format!(
                    "error[ops.argument]: one of the values isn't valid for an \
                     argument: `--log-level <LOG_LEVEL>`: `{value}` is not one of \
                     error, warn, info, debug, trace\n\
                     fix: Match the arguments to `foundation --help` or \
                     `foundation <operation> --help`\n"
                ))
            );
        }
    }

    #[test]
    fn a_log_format_out_of_the_two_is_an_argument_error_that_lists_them() {
        assert_eq!(
            cli(&["start", "--log-format", "logfmt"]),
            failed(
                "error[ops.argument]: one of the values isn't valid for an argument: \
                 `--log-format <LOG_FORMAT>`: `logfmt` is not one of text, json\n\
                 fix: Match the arguments to `foundation --help` or \
                 `foundation <operation> --help`\n"
            )
        );
    }

    #[test]
    fn a_bad_name_is_an_argument_error() {
        assert_eq!(
            cli(&["start", "--name", "a..b"]),
            failed(
                "error[ops.argument]: invalid value for one of the arguments: `--name \
                 <NAME>`: a segment is not valid: \"\" in \"a..b\"\n\
                 fix: Match the arguments to `foundation --help` or \
                 `foundation <operation> --help`\n"
            )
        );
    }

    #[test]
    fn a_listen_that_is_no_socket_address_is_an_argument_error() {
        assert_eq!(
            cli(&["start", "--listen", "x"]),
            failed(
                "error[ops.argument]: invalid value for one of the arguments: \
                 `--listen <LISTEN>`: invalid socket address syntax\n\
                 fix: Match the arguments to `foundation --help` or \
                 `foundation <operation> --help`\n"
            )
        );
    }

    #[test]
    fn the_help_of_start_names_its_flags() {
        let exit = cli(&["start", "--help"]);
        assert_eq!((exit.status, exit.stderr.as_str()), (0, ""));
        for flag in [
            "--data <DATA>",
            "--name <NAME>",
            "--listen <LISTEN>",
            "--json",
            "--log-level <LOG_LEVEL>",
            "--log-format <LOG_FORMAT>",
            "[default: foundation-data]",
            "[default: 127.0.0.1:0]",
            "[default: info]",
            "[possible values: error, warn, info, debug, trace]",
            "[possible values: text, json]",
            "`debug` and `trace` write lines for each packet and slow the node",
        ] {
            // clap wraps the help to the width of the terminal.
            let help = exit.stdout.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(help.contains(flag), "{flag}: {}", exit.stdout);
        }
    }

    /// `start` is a command, not an operation: the help names it, and the docs and
    /// the tools do not.
    #[test]
    fn the_help_names_start_and_the_docs_do_not() {
        let exit = cli(&["--help"]);
        assert_eq!((exit.status, exit.stderr.as_str()), (0, ""));
        assert!(
            exit.stdout.contains(
                "  start    Start a node on a data directory, and run it until Ctrl-C"
            ),
            "{}",
            exit.stdout
        );
        assert!(
            !crate::operation::docs(&crate::common::create_kinds()).contains("start")
        );
    }

    #[test]
    fn start_has_no_tool() {
        assert_eq!(
            call(&json!({ "name": "start", "arguments": {} }))["error"]["message"],
            "no operation is named `start`"
        );
    }

    #[test]
    fn line_gives_the_name_and_the_data_directory() {
        let mut start = Start {
            data: PathBuf::from("foundation-data"),
            json: false,
            name: None,
            listen: local(),
            log: Log::default(),
        };
        assert_eq!(
            start.line(&edge()),
            "node edge runs in foundation-data. Stop it with Ctrl-C.\n"
        );
        start.json = true;
        assert_eq!(
            start.line(&edge()),
            "{\"name\":\"edge\",\"data\":\"foundation-data\"}\n"
        );
    }

    #[test]
    fn line_escapes_the_data_directory_as_an_error_does() {
        let start = Start {
            data: PathBuf::from("a.\nnode cloud runs in b\\ce\u{301}"),
            json: false,
            name: None,
            listen: local(),
            log: Log::default(),
        };
        assert_eq!(
            start.line(&edge()),
            "node edge runs in a.\\nnode cloud runs in b\\\\ce\\u{301}. Stop it with \
             Ctrl-C.\n"
        );
    }

    #[test]
    fn line_and_fail_give_the_data_directory_in_one_form() {
        let data = "it's \"b\\c\"\ne\u{301}";
        let start = Start {
            data: PathBuf::from(data),
            json: false,
            name: None,
            listen: local(),
            log: Log::default(),
        };
        let shown = "it's \"b\\\\c\"\\ne\\u{301}";
        assert_eq!(
            start.line(&edge()),
            format!("node edge runs in {shown}. Stop it with Ctrl-C.\n")
        );
        let failure = Failure {
            code: Code::new("node.data"),
            message: format!("cannot write the data directory {data}: x"),
            fix: "y".to_owned(),
        };
        let mut text = Vec::new();
        assert_eq!(crate::fail(&[], &failure, &mut text), 1);
        assert_eq!(
            String::from_utf8(text).unwrap(),
            format!(
                "error[node.data]: cannot write the data directory {shown}: x\nfix: y\n"
            )
        );
    }

    #[cfg(unix)]
    #[test]
    fn line_gives_a_data_directory_that_is_not_utf8_lossily() {
        use std::os::unix::ffi::OsStrExt;

        let mut start = Start {
            data: PathBuf::from(std::ffi::OsStr::from_bytes(b"a\xf0\x9f\x98b\xffc")),
            json: true,
            name: None,
            listen: local(),
            log: Log::default(),
        };
        assert_eq!(
            start.line(&edge()),
            "{\"name\":\"edge\",\"data\":\"a\u{fffd}b\u{fffd}c\"}\n"
        );
        start.json = false;
        assert_eq!(
            start.line(&edge()),
            "node edge runs in a\u{fffd}b\u{fffd}c. Stop it with Ctrl-C.\n"
        );
    }

    /// A writer whose every call fails.
    struct Closed;

    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }

    fn args(args: &[&str]) -> Vec<OsString> {
        ["foundation"]
            .iter()
            .chain(args)
            .map(OsString::from)
            .collect()
    }

    #[test]
    fn fail_writes_the_failure_as_cli_does_and_gives_1() {
        let mut text = Vec::new();
        let status = crate::fail(&args(&["start"]), &busy(), &mut text);
        assert_eq!(
            Exit {
                stdout: String::new(),
                stderr: String::from_utf8(text).unwrap(),
                status,
            },
            Exit {
                stdout: String::new(),
                stderr: "error[node.busy]: another node runs in foundation-data\n\
                         fix: Stop that node, or give another data directory with \
                         `--data`\n"
                    .to_owned(),
                status: 1,
            }
        );
        let mut json = Vec::new();
        let status = crate::fail(&args(&["start", "--json"]), &busy(), &mut json);
        assert_eq!(status, 1);
        let json: serde_json::Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(
            json,
            json!({ "errors": [{
                "code": "node.busy",
                "message": "another node runs in foundation-data",
                "fix": "Stop that node, or give another data directory with `--data`",
                "notes": [],
            }] })
        );
        let status = crate::fail(&args(&["version", "--json"]), &busy(), Closed);
        assert_eq!(status, 1, "a closed output");
    }

    #[test]
    fn fail_after_a_double_dash_json_gives_text() {
        let mut text = Vec::new();
        let status =
            crate::fail(&args(&["version", "--", "--json"]), &busy(), &mut text);
        assert_eq!(status, 1);
        assert_eq!(
            String::from_utf8(text).unwrap(),
            "error[node.busy]: another node runs in foundation-data\n\
             fix: Stop that node, or give another data directory with `--data`\n"
        );
    }

    /// The OS gives the program name, so it can read as a flag.
    #[test]
    fn json_skips_the_program_name() {
        for (program, flags, json) in [
            ("--", &["start", "--json"][..], true),
            ("--json", &["start"][..], false),
        ] {
            let args: Vec<OsString> =
                [program].iter().chain(flags).map(OsString::from).collect();
            let run = offline(args.clone(), io::empty(), io::sink(), io::sink());
            let Run::Start(start) = run else {
                panic!("{args:?} starts a node");
            };
            let mut written = Vec::new();
            crate::fail(&args, &busy(), &mut written);
            assert_eq!(
                (start.json, written.starts_with(b"{")),
                (json, json),
                "{args:?}: {}",
                String::from_utf8_lossy(&written)
            );
        }
    }
}

mod mcp {
    use serde_json::{Value, json};

    use super::{ask, respond};
    use crate::mcp::GUIDE;
    use crate::operation::{self, TABLE};

    fn request(id: Value, method: &str, params: Value) -> Value {
        let mut request = json!({ "jsonrpc": "2.0", "method": method });
        request["id"] = id;
        request["params"] = params;
        request
    }

    fn failure(id: Value, code: i64, message: &str) -> Value {
        let mut failure =
            json!({ "jsonrpc": "2.0", "error": { "code": code, "message": message } });
        failure["id"] = id;
        failure
    }

    #[test]
    fn initialize_gives_the_version_and_the_tools_capability() {
        let reply = ask(&request(
            json!(1),
            "initialize",
            json!({ "protocolVersion": "2025-06-18" }),
        ));
        assert_eq!(
            reply,
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "foundation", "version": env!("CARGO_PKG_VERSION") },
                    "instructions": GUIDE,
                },
            })
        );
        assert!(GUIDE.starts_with("Run `docs` "), "{GUIDE}");
    }

    /// Each name in backticks in the guide is a tool, an input of one, or `fill`.
    #[test]
    fn the_guide_names_only_tools_their_inputs_and_fill() {
        let inputs = operation::inputs();
        let named: Vec<&str> = GUIDE.split('`').skip(1).step_by(2).collect();
        assert!(named.contains(&"import"), "{named:?}");
        for name in named {
            let input = inputs.values().any(|schema| {
                schema["properties"]
                    .as_object()
                    .is_some_and(|properties| properties.contains_key(name))
            });
            let tool = TABLE.iter().any(|spec| spec.name == name);
            assert!(tool || input || name == "fill", "`{name}` in the guide");
        }
    }

    #[test]
    fn initialize_offers_its_own_version_for_another() {
        let reply = ask(&request(
            json!("a"),
            "initialize",
            json!({ "protocolVersion": "1999-01-01" }),
        ));
        assert_eq!(reply["id"], "a");
        assert_eq!(reply["result"]["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn ping_gets_an_empty_result() {
        assert_eq!(
            ask(&json!({ "jsonrpc": "2.0", "id": 2, "method": "ping" })),
            json!({ "jsonrpc": "2.0", "id": 2, "result": {} })
        );
    }

    #[test]
    fn tools_list_and_call_answer_under_the_request_id() {
        let list = ask(&request(json!(3), "tools/list", json!({})));
        assert_eq!((&list["id"], &list["result"]), (&json!(3), &super::tools()));
        let call = ask(&request(
            json!("c"),
            "tools/call",
            json!({ "name": "version" }),
        ));
        assert_eq!(call["id"], "c");
        assert_eq!(
            call["result"]["structuredContent"]["version"],
            env!("CARGO_PKG_VERSION")
        );
    }

    #[test]
    fn a_response_from_the_client_gets_no_reply() {
        for response in [
            json!({ "jsonrpc": "2.0", "id": 1, "result": {} }),
            json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": 1, "message": "x" } }),
        ] {
            assert_eq!(respond(&response.to_string()), None, "{response}");
        }
    }

    #[test]
    fn an_id_that_parsing_could_change_is_invalid() {
        for id in ["18446744073709551616", "-0", "1.0", "1.5", "null"] {
            let message = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#);
            let reply: Value =
                serde_json::from_str(&respond(&message).expect("a reply"))
                    .expect("json");
            assert_eq!(
                reply,
                failure(Value::Null, -32600, "Invalid Request"),
                "{id}"
            );
        }
        for id in [json!(u64::MAX), json!(i64::MIN), json!("")] {
            assert_eq!(ask(&request(id.clone(), "ping", json!({})))["id"], id);
        }
    }

    #[test]
    fn a_notification_gets_no_reply() {
        let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        assert_eq!(respond(&note.to_string()), None);
        let unknown = json!({ "jsonrpc": "2.0", "method": "nope", "params": 5 });
        assert_eq!(respond(&unknown.to_string()), None);
    }

    #[test]
    fn an_invalid_notification_gets_a_reply() {
        assert_eq!(
            ask(&json!({ "jsonrpc": "2.0", "method": 1, "params": "bar" })),
            failure(Value::Null, -32600, "Invalid Request")
        );
    }

    #[test]
    fn bad_json_is_a_parse_error() {
        let reply: Value =
            serde_json::from_str(&respond("{").expect("a reply")).expect("json");
        assert_eq!(reply, failure(Value::Null, -32700, "Parse error"));
    }

    #[test]
    fn a_message_that_is_not_a_request_is_invalid() {
        assert_eq!(
            ask(&json!([1])),
            failure(Value::Null, -32600, "Invalid Request")
        );
        assert_eq!(
            ask(&json!({ "id": 1, "method": "ping" })),
            failure(json!(1), -32600, "Invalid Request")
        );
        assert_eq!(
            ask(&json!({ "jsonrpc": "2.0", "id": true, "method": "ping" })),
            failure(Value::Null, -32600, "Invalid Request")
        );
        assert_eq!(
            ask(&json!({ "jsonrpc": "2.0", "id": 7 })),
            failure(json!(7), -32600, "Invalid Request")
        );
    }

    #[test]
    fn an_unknown_method_is_not_found() {
        assert_eq!(
            ask(&request(json!(8), "nope", json!({}))),
            failure(json!(8), -32601, "Method not found")
        );
    }

    #[test]
    fn bad_call_params_are_invalid() {
        assert_eq!(
            ask(&request(json!(9), "ping", json!(5))),
            failure(json!(9), -32602, "Invalid params")
        );
        for params in [
            Value::Null,
            json!([]),
            json!({}),
            json!({ "name": 1 }),
            json!({ "name": "version", "arguments": [] }),
            json!({ "name": "version", "arguments": null }),
        ] {
            assert_eq!(
                ask(&request(json!(9), "tools/call", params.clone())),
                failure(json!(9), -32602, "Invalid params"),
                "{params}"
            );
        }
    }

    #[test]
    fn a_request_that_also_has_a_result_is_answered() {
        let mut message = request(json!(1), "ping", json!({}));
        message["result"] = json!({});
        assert_eq!(ask(&message)["result"], json!({}));
    }

    #[test]
    fn an_unknown_method_with_positional_params_is_not_found() {
        assert_eq!(
            ask(&request(json!(8), "nope", json!([1, 2]))),
            failure(json!(8), -32601, "Method not found")
        );
    }

    #[test]
    fn a_tool_call_suggests_only_operations() {
        for name in ["mcpp", "hepl", "-h", "--json"] {
            assert_eq!(
                ask(&request(json!(1), "tools/call", json!({ "name": name })))["error"]
                    ["data"]["errors"][0]["fix"],
                "Use a name from `tools/list`",
                "{name}"
            );
        }
        assert_eq!(
            ask(&request(
                json!(1),
                "tools/call",
                json!({ "name": "versoin" })
            ))["error"]["data"]["errors"][0]["fix"],
            "Use `version`, the closest name"
        );
    }
}

mod serve {
    use std::io::{self, Read, Write};

    use super::{Exit, offline, run};
    use crate::Run;

    const PING: &str = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n";
    const PONG: &str = "{\"id\":1,\"jsonrpc\":\"2.0\",\"result\":{}}\n";

    struct Failing(io::ErrorKind);

    impl Read for Failing {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(self.0.into())
        }
    }

    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(self.0.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A last line of exactly 1 MiB with no `\n` is not longer than 1 MiB.
    #[test]
    fn a_last_line_of_the_limit_with_no_newline_runs() {
        let ping = PING.trim_end();
        let last = format!("{}{ping}", " ".repeat(crate::mcp::LIMIT - ping.len()));
        let line = crate::line(&mut last.as_bytes()).unwrap();
        assert_eq!(line, crate::Line::Bytes(last.clone().into_bytes()));
        assert_eq!(run(&["mcp"], last.as_bytes()).stdout, PONG);
    }

    #[test]
    fn fuzz_inputs_reach_their_codes() {
        let root = include_str!("../../../oracles/fuzz/ops_mcp/plan-root");
        let out = run(&["mcp"], root.as_bytes()).stdout;
        assert!(out.contains("ops.outside"), "{out}");
        let outside = include_str!("../../../oracles/fuzz/ops_mcp/plan-outside");
        let out = run(&["mcp"], outside.as_bytes()).stdout;
        assert!(out.contains("ops.outside"), "{out}");
        let missing = include_str!("../../../oracles/fuzz/ops_mcp/plan-missing");
        let out = run(&["mcp"], missing.as_bytes()).stdout;
        assert!(out.contains("ops.read"), "{out}");
        let directory = include_str!("../../../oracles/fuzz/ops_mcp/plan-directory");
        let out = run(&["mcp"], directory.as_bytes()).stdout;
        assert!(out.contains("ops.no-config"), "{out}");
    }

    #[test]
    fn mcp_answers_each_line_until_input_ends() {
        let note = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n";
        let input = format!("{PING}{note}{PING}");
        assert_eq!(
            run(&["mcp"], input.as_bytes()),
            Exit {
                stdout: format!("{PONG}{PONG}"),
                stderr: String::new(),
                status: 0
            }
        );
    }

    #[test]
    fn a_line_that_is_not_utf8_gets_the_parse_error_and_the_next_line_runs() {
        let input = [b"{\"method\":\"\xff\"}\n".as_slice(), PING.as_bytes()].concat();
        let exit = run(&["mcp"], input.as_slice());
        assert_eq!(
            exit.stdout,
            format!(
                "{}\n{PONG}",
                r#"{"error":{"code":-32700,"message":"Parse error"},"id":null,"jsonrpc":"2.0"}"#
            )
        );
        assert_eq!((exit.status, exit.stderr.as_str()), (0, ""));
    }

    /// Keeps written bytes until a flush. A closed writer fails each flush.
    #[derive(Default)]
    struct Buffered {
        pending: Vec<u8>,
        flushed: Vec<u8>,
        closed: bool,
    }

    impl Write for Buffered {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.pending.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.closed {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            self.flushed.append(&mut self.pending);
            Ok(())
        }
    }

    #[test]
    fn each_reply_is_flushed() {
        let mut output = Buffered::default();
        let status = offline(
            ["foundation", "mcp"].map(Into::into),
            PING.as_bytes(),
            &mut output,
            io::sink(),
        );
        assert_eq!(status, Run::Exit(0));
        assert_eq!(
            (output.pending.as_slice(), output.flushed.as_slice()),
            (b"".as_slice(), PONG.as_bytes())
        );
    }

    #[test]
    fn a_flush_to_a_reader_that_left_ends_the_run_without_an_error() {
        let input = format!("{PING}{PING}");
        let mut rest = input.as_bytes();
        let mut stderr = Vec::new();
        let output = Buffered {
            closed: true,
            ..Buffered::default()
        };
        let status = offline(
            ["foundation", "mcp"].map(Into::into),
            &mut rest,
            output,
            &mut stderr,
        );
        assert_eq!(
            (status, stderr.as_slice(), rest),
            (Run::Exit(0), b"".as_slice(), PING.as_bytes())
        );
    }

    #[test]
    fn a_line_past_the_limit_is_invalid_and_the_next_line_runs() {
        let long = format!("{}\n", " ".repeat(crate::mcp::LIMIT));
        let most = format!("{}{PING}", " ".repeat(crate::mcp::LIMIT - PING.len()));
        let exit = run(&["mcp"], format!("{long}{most}{PING}").as_bytes());
        assert_eq!(
            exit.stdout,
            format!(
                "{}\n{PONG}{PONG}",
                r#"{"error":{"code":-32600,"message":"Invalid Request"},"id":null,"jsonrpc":"2.0"}"#
            )
        );
        assert_eq!((exit.status, exit.stderr.as_str()), (0, ""));
    }

    #[test]
    fn a_reader_that_left_ends_the_run_without_an_error() {
        let input = format!("{PING}{PING}");
        let mut rest = input.as_bytes();
        let mut stdout = Vec::new();
        let status = offline(
            ["foundation", "mcp"].map(Into::into),
            &mut rest,
            Failing(io::ErrorKind::BrokenPipe),
            &mut stdout,
        );
        assert_eq!(
            (status, stdout.as_slice(), rest),
            (Run::Exit(0), b"".as_slice(), PING.as_bytes())
        );
        let mut stderr = Vec::new();
        let status = offline(
            ["foundation", "version"].map(Into::into),
            io::empty(),
            Failing(io::ErrorKind::BrokenPipe),
            &mut stderr,
        );
        assert_eq!((status, stderr.as_slice()), (Run::Exit(0), b"".as_slice()));
    }

    #[test]
    fn an_output_that_fails_is_an_error() {
        let mut stderr = Vec::new();
        let status = offline(
            ["foundation", "version", "--json"].map(Into::into),
            io::empty(),
            Failing(io::ErrorKind::StorageFull),
            &mut stderr,
        );
        let message = io::Error::from(io::ErrorKind::StorageFull).to_string();
        assert_eq!(status, Run::Exit(1));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stderr).expect("json"),
            serde_json::json!({ "errors": [{
                "code": "ops.output",
                "message": format!("standard output could not be written: {message}"),
                "fix": "Give standard output a destination that can be written",
                "notes": [],
            }] })
        );
    }

    #[test]
    fn an_input_that_fails_is_an_error() {
        let input = io::BufReader::new(Failing(io::ErrorKind::Other));
        let message = io::Error::from(io::ErrorKind::Other).to_string();
        assert_eq!(
            run(&["mcp"], input),
            Exit {
                stdout: String::new(),
                stderr: format!(
                    "error[ops.input]: standard input could not be read: {message}\n\
                     fix: Give standard input a source that can be read\n"
                ),
                status: 1
            }
        );
    }

    #[test]
    fn help_lists_mcp_and_describes_foundation() {
        let help = run(&["--help"], io::empty()).stdout;
        assert!(help.starts_with("Run Foundation operations\n"), "{help}");
        assert!(
            help.lines()
                .any(|line| line.trim_start().starts_with("mcp ")),
            "{help}"
        );
    }
}

#[test]
fn the_places_of_one_file_share_its_path() {
    let files = crate::common::files(&[("site.hcl", "nope \"a\" {}\nnope \"b\" {}\n")]);
    let read = crate::plan::Documents::read(&files, &crate::common::front_ends())
        .and_then(|documents| documents.names(&crate::common::create_kinds()));
    let Err(Error::Config(problems)) = read else {
        panic!("two problems, not {read:?}");
    };
    let [a, b] = &problems[..] else {
        panic!("two problems, not {problems:?}");
    };
    let (Some(a), Some(b)) = (&a.place, &b.place) else {
        panic!("two places, not {problems:?}");
    };
    assert_eq!(&*a.file, "site.hcl");
    assert!(std::sync::Arc::ptr_eq(&a.file, &b.file));
}

/// The pointer that follows the problems of a body.
const DOCS: &str =
    "Run `foundation docs` for the attributes and blocks that each block takes.\n";

/// The terminal text of the problems that the check gives for one file of `text`.
/// Panics when the JSON answer holds more than the problems or names the docs.
fn checked_text(text: &str) -> String {
    let files = crate::common::files(&[("site.hcl", text)]);
    let read = crate::plan::Documents::read(&files, &crate::common::front_ends())
        .and_then(|documents| documents.names(&crate::common::create_kinds()));
    let Err(error) = read else {
        panic!("a problem, not {read:?}");
    };
    assert_eq!(error.json(), json!({ "errors": error.problems() }));
    let json = error.json().to_string();
    assert!(!json.contains("foundation docs"), "{json}");
    error.text()
}

#[test]
fn a_problem_of_a_key_or_block_of_a_body_points_at_the_docs_once() {
    let time = "channel \"t\" {\n  kind = \"index\"\n}\n";
    for (text, code) in [
        (
            "nope \"a\" {}\nnope \"b\" {}\n".to_owned(),
            "document.unknown-block",
        ),
        (
            format!(
                "{time}channel \"p\" {{\n  data_type = \"f64\"\n  index = \"t\"\n  \
                 colour = 1\n}}\n"
            ),
            "document.unknown-attribute",
        ),
        (
            format!("{time}channel \"p\" {{\n  index = \"t\"\n}}\n"),
            "document.missing-attribute",
        ),
    ] {
        let text = checked_text(&text);
        assert!(text.starts_with(&format!("error[{code}]: ")), "{text}");
        assert!(text.ends_with(&format!("\n\n{DOCS}")), "{text}");
        assert_eq!(text.matches(DOCS).count(), 1, "{text}");
    }
    let text = checked_text(&format!(
        "{time}channel \"p\" {{\n  data_type = \"nope\"\n  index = \"t\"\n}}\n"
    ));
    assert!(text.starts_with("error[config.bad-data-type]: "), "{text}");
    assert!(!text.contains("foundation docs"), "{text}");
}

#[test]
fn the_data_directory_of_one_name_is_in_the_working_directory() {
    let error = Error::Data {
        path: PathBuf::from("foundation-data"),
        error: env::paths::Error::NotFound,
    };
    assert_eq!(
        error.to_string(),
        "the directory . above the data directory `foundation-data` is not there"
    );
}

mod scope {
    use std::path::{Path, PathBuf};

    use crate::error::Error;
    use crate::scope::Scope;

    /// A host with the folder `/work`, which is the working directory, and the scope
    /// of `foundation mcp` there, with the data directory `/work/foundation-data`.
    fn work() -> (sim::node::Node, Scope) {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = sim.node(sim::node::Config::default());
        host.make_dir(Path::new("/work"));
        host.set_working_dir(Path::new("/work"));
        let data = Path::new("/work/foundation-data");
        let scope = Scope::mcp(&host.paths(), &crate::common::front_ends(), &[], data)
            .expect("a scope");
        (host, scope)
    }

    #[test]
    fn a_plan_file_by_a_data_directory_moved_away_has_no_data() {
        let (host, scope) = work();
        let data = Path::new("/work/foundation-data");
        host.move_path(data, Path::new("/data"));
        assert_eq!(
            scope.write(Path::new("out.plan"), b"x"),
            Err(Error::NoData {
                path: PathBuf::from("out.plan"),
                data: data.to_owned(),
            })
        );
    }

    #[test]
    fn a_plan_file_in_a_folder_made_again_is_in_a_changed_folder() {
        let (host, scope) = work();
        host.move_path(Path::new("/work"), Path::new("/old"));
        host.make_dir(Path::new("/work"));
        assert_eq!(
            scope.write(Path::new("out.plan"), b"x"),
            Err(Error::FolderChanged {
                path: PathBuf::from("out.plan"),
                folders: vec![PathBuf::from("/work")],
            })
        );
    }

    #[test]
    fn a_plan_file_in_a_files_folder_made_again_is_in_a_changed_folder() {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = sim.node(sim::node::Config::default());
        host.make_dir(Path::new("/plans"));
        let data = Path::new("/data");
        let scope = Scope::mcp(
            &host.paths(),
            &crate::common::front_ends(),
            &["/plans".into()],
            data,
        )
        .expect("a scope");
        host.move_path(Path::new("/plans"), Path::new("/old"));
        host.make_dir(Path::new("/plans"));
        assert_eq!(
            scope.write(Path::new("/plans/out.plan"), b"x"),
            Err(Error::FolderChanged {
                path: PathBuf::from("/plans/out.plan"),
                folders: vec![PathBuf::from("/plans")],
            })
        );
    }
}
