use std::collections::BTreeSet;
use std::ffi::OsString;

use serde_json::{Map, Value, json};

use crate::Exit;
use crate::error::Error;
use crate::operation::{self, Response, TABLE};

fn cli(args: &[&str]) -> Exit {
    let args = ["foundation"].iter().chain(args).map(OsString::from);
    crate::cli(args)
}

fn failed(stderr: &str) -> Exit {
    Exit {
        stdout: String::new(),
        stderr: stderr.to_owned(),
        status: 2,
    }
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
    let tools = crate::tools();
    let tools = tools["tools"].as_array().expect("tools is a list");
    let docs = operation::docs();
    assert_eq!(command.get_subcommands().count(), TABLE.len());
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
        let output: Value = serde_json::from_str(&exit.stdout).expect("json");
        let response: Response =
            serde_json::from_value(json!({ spec.name: output.clone() }))
                .expect(spec.name);
        assert_eq!(response.json(), output);
        let call = crate::call(spec.name, json!({}));
        assert_eq!(call["isError"], false);
        assert_eq!(call["structuredContent"], output);
        assert_eq!(call["content"][0]["text"], output.to_string());
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
            stdout: operation::docs(),
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
             fix: Match the arguments to the operation in `foundation docs`\n"
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
        json!({
            "code": "ops.argument",
            "message": "unexpected argument found: `--nope`",
            "fix": "Match the arguments to the operation in `foundation docs`",
        })
    );
}

#[test]
fn no_operation_gives_the_same_error_as_text_and_as_json() {
    let message = "a subcommand is required but one was not provided";
    let fix = "Match the arguments to the operation in `foundation docs`";
    assert_eq!(
        cli(&[]),
        failed(&format!("error[ops.argument]: {message}\nfix: {fix}\n"))
    );
    let json = cli(&["--json"]);
    let error: Value = serde_json::from_str(&json.stderr).expect("json");
    assert_eq!(
        error,
        json!({ "code": "ops.argument", "message": message, "fix": fix })
    );
}

#[test]
fn json_after_a_double_dash_is_not_the_flag() {
    assert_eq!(
        cli(&["version", "--", "--json"]),
        failed(
            "error[ops.argument]: unexpected argument found: `--json`\n\
             fix: Match the arguments to the operation in `foundation docs`\n"
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
    let call = crate::call("versoin", json!({}));
    assert_eq!(call["isError"], true);
    assert_eq!(
        call["structuredContent"],
        json!({
            "code": "ops.unknown-operation",
            "message": "no operation is named `versoin`",
            "fix": "Use `version`, the closest name",
        })
    );
}

#[test]
fn an_unknown_operation_far_from_every_name_points_to_the_docs() {
    let expected = json!({
        "code": "ops.unknown-operation",
        "message": "no operation is named `zzz`",
        "fix": "Use a name from `foundation docs`",
    });
    assert_eq!(crate::call("zzz", json!({}))["structuredContent"], expected);
    let exit = cli(&["zzz", "--json"]);
    assert_eq!((exit.status, exit.stdout.as_str()), (2, ""));
    assert_eq!(
        serde_json::from_str::<Value>(&exit.stderr).expect("json"),
        expected
    );
}

#[test]
fn a_call_with_a_bad_argument_names_it() {
    let call = crate::call("version", json!({ "nope": 1 }));
    assert_eq!(call["isError"], true);
    assert_eq!(
        call["structuredContent"],
        json!({
            "code": "ops.argument",
            "message": "unknown field `nope`, there are no fields",
            "fix": "Match the arguments to the operation in `foundation docs`",
        })
    );
    assert_eq!(
        call["content"][0]["text"],
        call["structuredContent"].to_string()
    );
}

#[test]
fn a_call_with_no_arguments_runs() {
    let call = crate::call("version", Value::Null);
    assert_eq!(call["isError"], false);
    assert_eq!(
        call["structuredContent"]["version"],
        env!("CARGO_PKG_VERSION")
    );
}

#[test]
fn error_codes_and_fixes_match_the_golden_file() {
    let every = [
        Error::Argument {
            message: String::new(),
        },
        Error::Unknown {
            name: String::new(),
            closest: None,
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
    assert_eq!(lines.concat(), include_str!("codes.golden"));
}
