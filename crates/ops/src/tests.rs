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

fn ask(message: &Value) -> Value {
    let reply = crate::mcp(&message.to_string()).expect("a reply");
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
        let call = call(&json!({ "name": spec.name, "arguments": {} }));
        assert_eq!(
            call["result"],
            json!({
                "content": [{ "type": "text", "text": output.to_string() }],
                "structuredContent": output,
            })
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
    assert_eq!(
        call(&json!({ "name": "versoin" })),
        failed_call(
            "no operation is named `versoin`",
            &json!({
                "code": "ops.unknown-operation",
                "message": "no operation is named `versoin`",
                "fix": "Use `version`, the closest name",
            })
        )
    );
}

#[test]
fn an_unknown_operation_far_from_every_name_points_to_the_docs() {
    let expected = json!({
        "code": "ops.unknown-operation",
        "message": "no operation is named `zzz`",
        "fix": "Use a name from `foundation docs`",
    });
    assert_eq!(
        call(&json!({ "name": "zzz" })),
        failed_call("no operation is named `zzz`", &expected)
    );
    let exit = cli(&["zzz", "--json"]);
    assert_eq!((exit.status, exit.stdout.as_str()), (2, ""));
    assert_eq!(
        serde_json::from_str::<Value>(&exit.stderr).expect("json"),
        expected
    );
}

#[test]
fn a_call_with_a_bad_argument_names_it() {
    let message = "unknown field `nope`, there are no fields";
    assert_eq!(
        call(&json!({ "name": "version", "arguments": { "nope": 1 } })),
        failed_call(
            message,
            &json!({
                "code": "ops.argument",
                "message": message,
                "fix": "Match the arguments to the operation in `foundation docs`",
            })
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

mod mcp {
    use serde_json::{Value, json};

    use super::ask;

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
                },
            })
        );
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
            assert_eq!(crate::mcp(&response.to_string()), None, "{response}");
        }
    }

    #[test]
    fn an_id_that_parsing_could_change_is_invalid() {
        for id in ["18446744073709551616", "-0", "1.0", "1.5", "null"] {
            let message = format!(r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#);
            let reply: Value =
                serde_json::from_str(&crate::mcp(&message).expect("a reply"))
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
        assert_eq!(crate::mcp(&note.to_string()), None);
        let unknown = json!({ "jsonrpc": "2.0", "method": "nope", "params": 5 });
        assert_eq!(crate::mcp(&unknown.to_string()), None);
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
            serde_json::from_str(&crate::mcp("{").expect("a reply")).expect("json");
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
}
