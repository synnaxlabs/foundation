//! Runs the built `foundation` binary as a process.

#![cfg(test)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Output, Stdio};

fn foundation(args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_foundation"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write");
    child.wait_with_output().expect("wait")
}

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("UTF-8")
}

#[test]
fn an_operation_prints_its_output_and_exits_0() {
    let output = foundation(&["version"], "");
    assert_eq!(
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        ),
        (
            Some(0),
            format!("{}\n", env!("CARGO_PKG_VERSION")).as_str(),
            ""
        )
    );
}

#[test]
fn an_error_goes_to_standard_error_and_exits_2() {
    let output = foundation(&["versoin"], "");
    assert_eq!(
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        ),
        (
            Some(2),
            "",
            "error[ops.unknown-operation]: no operation is named `versoin`\n\
             fix: Use `version`, the closest name\n"
        )
    );
}

#[test]
fn mcp_answers_each_request_on_its_own_line_until_input_closes() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        r#"{"jsonrpc":"2.0","id":"b","method":"nope"}"#,
        "\n",
    );
    let output = foundation(&["mcp"], input);
    assert_eq!(
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        ),
        (
            Some(0),
            concat!(
                r#"{"id":1,"jsonrpc":"2.0","result":{}}"#,
                "\n",
                r#"{"error":{"code":-32601,"message":"Method not found"},"id":"b","jsonrpc":"2.0"}"#,
                "\n",
            ),
            ""
        )
    );
}

#[test]
fn mcp_replies_before_the_next_request_arrives() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_foundation"))
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut stdin = child.stdin.take().expect("stdin");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout"));
    for id in 1..=2 {
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":{id},"method":"ping"}}"#)
            .expect("write");
        let mut line = String::new();
        stdout.read_line(&mut line).expect("read");
        assert_eq!(
            line,
            format!("{{\"id\":{id},\"jsonrpc\":\"2.0\",\"result\":{{}}}}\n")
        );
    }
    drop(stdin);
    assert_eq!(child.wait().expect("wait").code(), Some(0));
}

#[test]
fn mcp_answers_a_line_that_is_not_utf8_and_goes_on() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_foundation"))
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let input = [
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"\xff\"}\n".as_slice(),
        b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}\n",
    ]
    .concat();
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(&input)
        .expect("write");
    let output = child.wait_with_output().expect("wait");
    assert_eq!(
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        ),
        (
            Some(0),
            concat!(
                r#"{"error":{"code":-32700,"message":"Parse error"},"id":null,"jsonrpc":"2.0"}"#,
                "\n",
                r#"{"id":2,"jsonrpc":"2.0","result":{}}"#,
                "\n",
            ),
            ""
        )
    );
}

#[test]
fn help_lists_mcp() {
    let output = foundation(&["--help"], "");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        text(&output.stdout)
            .lines()
            .any(|line| line.trim_start().starts_with("mcp ")),
        "{}",
        text(&output.stdout)
    );
}
