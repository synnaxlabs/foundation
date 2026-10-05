use std::collections::BTreeSet;
use std::ffi::OsString;

use serde_json::{Value, json};

use crate::operation::{Response, TABLE};
use crate::{Error, Exit};

fn cli(args: &[&str]) -> Exit {
    let args = ["foundation"].iter().chain(args).map(OsString::from);
    crate::cli(args)
}

#[test]
fn each_operation_appears_once_in_the_cli_the_tools_and_the_docs() {
    let command = crate::operation::command();
    let tools = crate::tools();
    let tools = tools["tools"].as_array().expect("tools is a list");
    let docs = crate::docs();
    let names: BTreeSet<_> = TABLE.iter().map(|spec| spec.name).collect();
    assert_eq!(names.len(), TABLE.len(), "names are unique");
    assert_eq!(command.get_subcommands().count(), TABLE.len());
    assert_eq!(tools.len(), TABLE.len());
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
        assert_eq!(tool[0]["inputSchema"]["type"], "object", "{}", spec.name);
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

fn yes(flag: bool) -> &'static str {
    if flag { "yes" } else { "no" }
}

#[test]
fn json_output_parses_back_to_the_typed_output() {
    for spec in TABLE {
        let exit = cli(&[spec.name, "--json"]);
        assert_eq!(
            (exit.status, exit.stderr.as_str()),
            (0, ""),
            "{}",
            spec.name
        );
        let response = Response::parse(spec.name, &exit.stdout).expect(spec.name);
        let call = crate::call(spec.name, json!({})).expect(spec.name);
        assert_eq!(serde_json::to_value(&response).expect("serializes"), call);
        assert_eq!(
            serde_json::from_str::<Value>(&exit.stdout).expect("json"),
            call
        );
    }
}

#[test]
fn version_prints_the_crate_version() {
    let version = env!("CARGO_PKG_VERSION");
    let text = cli(&["version"]);
    assert_eq!(
        (text.status, text.stdout.as_str()),
        (0, format!("{version}\n").as_str())
    );
    let json = cli(&["version", "--json"]);
    assert_eq!(json.stdout, format!("{{\"version\":\"{version}\"}}\n"));
}

#[test]
fn docs_prints_the_reference() {
    let exit = cli(&["docs"]);
    assert_eq!(
        exit,
        Exit {
            stdout: crate::docs(),
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
    let exit = cli(&["version", "--nope"]);
    assert_eq!(
        exit,
        Exit {
            stdout: String::new(),
            stderr: "error[ops.argument]: unexpected argument '--nope' found\n\
                     fix: Match the arguments to the operation in `foundation docs`\n"
                .to_owned(),
            status: 2,
        }
    );
}

#[test]
fn a_bad_argument_gives_the_code_message_and_fix_as_json() {
    let exit = cli(&["version", "--nope", "--json"]);
    assert_eq!((exit.status, exit.stdout.as_str()), (2, ""));
    let error: Value = serde_json::from_str(&exit.stderr).expect("json");
    assert_eq!(
        error,
        json!({
            "code": "ops.argument",
            "message": "unexpected argument '--nope' found",
            "fix": "Match the arguments to the operation in `foundation docs`",
        })
    );
}

#[test]
fn an_unknown_operation_on_the_command_line_is_a_bad_argument() {
    let exit = cli(&["nope"]);
    assert_eq!(exit.status, 2);
    assert!(
        exit.stderr
            .starts_with("error[ops.argument]: unrecognized subcommand 'nope'\n"),
        "{}",
        exit.stderr
    );
}

#[test]
fn a_call_to_an_unknown_tool_names_it() {
    let error = crate::call("nope", json!({})).expect_err("unknown");
    assert_eq!(
        error,
        Error::Unknown {
            name: "nope".to_owned()
        }
    );
    assert_eq!(error.to_string(), "no operation is named `nope`");
    assert_eq!(error.code().as_str(), "ops.unknown-operation");
    assert_eq!(error.fix(), "Use a name from `foundation docs`");
}

#[test]
fn a_call_with_a_bad_argument_names_it() {
    let error = crate::call("version", json!({ "nope": 1 })).expect_err("bad argument");
    assert_eq!(
        error,
        Error::Argument {
            message: "unknown field `nope`, there are no fields".to_owned()
        }
    );
    assert_eq!(error.code().as_str(), "ops.argument");
}

#[test]
fn error_codes_and_fixes_match_the_golden_file() {
    let every = [
        Error::Argument {
            message: String::new(),
        },
        Error::Unknown {
            name: String::new(),
        },
    ];
    for error in &every {
        // A new variant fails this match, so it joins `every` and the golden file.
        match error {
            Error::Argument { .. } | Error::Unknown { .. } => {}
        }
    }
    let lines: Vec<_> = every
        .iter()
        .map(|error| format!("{}\t{}\n", error.code(), error.fix()))
        .collect();
    let lines = lines.concat();
    assert_eq!(lines, include_str!("codes.golden"));
}
