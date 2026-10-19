use std::path::Path;

use document::diagnostic::Diagnostic;
use env::paths::Paths;
use serde_json::{Value, json};
use types::name::Name;
use types::sample;
use wire::hub::client::BODY_BYTES_MAX;

use super::Output;
use crate::common::{self, EVE, create_browsing_node, more_problems, name, solo};
use crate::error::{Error, Sources};
use crate::serve::serve;
use crate::{Failure, Node, Run};

/// The text of a file with the connector `plant` of `kind` on `node`, and `more`
/// lines in its block.
fn site(kind: &str, node: &str, more: &str) -> String {
    let head = format!("connector \"plant\" {{\n  kind    = \"{kind}\"\n");
    format!("{head}  node    = \"{node}\"\n{more}}}\n")
}

/// The body of a `browse` request of `connector` in the file `path` of `text`, with
/// each field of `query`.
fn body(connector: &str, path: &str, text: &str, query: &Value) -> Vec<u8> {
    let mut browse = json!({
        "connector": connector,
        "files": [{ "path": path, "text": text }],
    });
    for (key, value) in query.as_object().expect("an object") {
        browse[key] = value.clone();
    }
    format!("{}\n", json!({ "browse": browse })).into_bytes()
}

/// The reply of `serve` to `body` as `subject`, as JSON. It asserts that the reply is
/// one line.
async fn served(node: &Node, subject: &str, body: &[u8]) -> Value {
    let reply = serve(node, &name(subject), body).await;
    let (line, rest) = reply.split_at(reply.len() - 1);
    assert_eq!(rest, b"\n");
    assert!(!line.contains(&b'\n'));
    serde_json::from_slice(line).expect("JSON")
}

/// The output of a browse of `plant` of `kind` on `edge` as `@admin`, with `query`.
async fn browsed(node: &Node, kind: &str, query: &Value) -> Value {
    let site = site(kind, "edge", "");
    let reply = served(node, "@admin", &body("plant", "site.hcl", &site, query)).await;
    let output = reply["output"].clone();
    assert!(!output.is_null(), "{reply}");
    output
}

/// Applies the file `people.hcl` of `text` on `node` as `@admin`.
async fn applied(node: &Node, text: &str) {
    let plan = json!({ "plan": { "files": [{ "path": "people.hcl", "text": text }] } });
    let reply = serve(node, &name("@admin"), format!("{plan}\n").as_bytes()).await;
    let end = reply
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("a first line");
    let apply = [&b"{\"apply\":{}}\n"[..], &reply[end + 1..]].concat();
    let reply = served(node, "@admin", &apply).await;
    assert!(reply.get("output").is_some(), "{reply}");
}

/// The reply of one error with no place.
fn refusal(code: &str, status: u8, message: &str, fix: &str) -> Value {
    json!({
        "errors": [{ "code": code, "message": message, "fix": fix, "notes": [] }],
        "status": status,
    })
}

/// The reply of `ops.large-browse`.
fn large_browse() -> Value {
    let message = format!(
        "the reply of the browse has more than {BODY_BYTES_MAX} bytes, the most that a \
         reply holds"
    );
    let fix = "Narrow the start or the match, or browse without `values`";
    refusal("ops.large-browse", 1, &message, fix)
}

/// The message of a failed browse of `plant` of `kind`.
fn failed(kind: &str, message: &str) -> String {
    format!("the browse of the connector `plant` of kind `{kind}` failed: {message}")
}

/// A point of `facts` at `path` with the tag `tag`.
fn point(path: &str, tag: &str, facts: &Value) -> Value {
    let mut point = json!({ "path": path, "tag": tag, "shape": "point" });
    for (key, value) in facts.as_object().expect("an object") {
        point[key] = value.clone();
    }
    point
}

/// The facts of a point that the device knows nothing of.
fn unknown() -> Value {
    json!({ "type": null, "unit": null, "unit_code": null, "access": null })
}

/// The text of `output`, after it parses back to an [`Output`].
fn text(output: &Value) -> String {
    let output: Output = serde_json::from_value(output.clone()).expect("an output");
    output.text()
}

/// The paths of a host whose working directory holds `site.hcl` with the connector
/// `plant` of `kind` on `edge`.
fn host(fixture: &common::Fixture, kind: &str) -> Paths {
    fixture.sim.make_dir(Path::new("/work"));
    fixture.sim.set_working_dir(Path::new("/work"));
    let paths = fixture.sim.paths();
    let site = site(kind, "edge", "");
    paths
        .write(Path::new("site.hcl"), site.as_bytes())
        .expect("a write");
    paths
}

/// The run, output, and errors of `foundation <args>` on `node`, which serves each
/// request as `@admin`.
async fn cli(node: &Node, paths: &Paths, args: &[&str]) -> (Run, String, String) {
    let reach = async |_: &Path| {
        Ok::<_, Failure>(async |body: &[u8]| {
            Ok::<_, Failure>(serve(node, &name("@admin"), body).await)
        })
    };
    common::cli(paths, args, reach).await
}

/// Each check fails in its order: each request fails each later check too.
#[test]
fn checks_the_files_the_name_access_the_node_the_start_and_the_match_in_order() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let query = json!({ "start": "a//b", "match": "a\\", "cursor": "0" });
        let other = site("tree", "other", "");
        let reply =
            served(&node, "eve", &body("x y", "site.yaml", &other, &query)).await;
        let expected = json!({
            "errors": [{
                "code": "ops.unknown-extension",
                "message": "no config syntax reads this file",
                "fix": "Use a file that ends in `.hcl`",
                "place": { "file": "site.yaml", "line": 1, "column": 1 },
                "notes": [],
            }],
            "status": 2,
        });
        assert_eq!(reply, expected);

        let reply =
            served(&node, "eve", &body("x y", "site.hcl", &other, &query)).await;
        let cause = "x y".parse::<Name>().expect_err("no name");
        let message = format!("the connector `x y` is not a name: {cause}");
        let fix = "Match the arguments to `foundation --help` or `foundation \
                   <operation> --help`";
        assert_eq!(reply, refusal("ops.argument", 2, &message, fix));

        let reply =
            served(&node, "eve", &body("gone", "site.hcl", &other, &query)).await;
        let message = "no `connector` block of the files has the name \"gone\"";
        let fix = "Use one of [\"plant\"]";
        assert_eq!(reply, refusal("config.no-connector", 2, message, fix));

        let reply =
            served(&node, "eve", &body("plant", "site.hcl", &other, &query)).await;
        let message = "no access policy gives `eve` the action `apply` on `plant`";
        let fix = "Ask an admin of the region for an access policy that allows it";
        assert_eq!(reply, refusal("ops.denied", 1, message, fix));

        let at = |text: &str, query: &Value| body("plant", "site.hcl", text, query);
        let reply = served(&node, "@admin", &at(&other, &query)).await;
        let message = "the connector `plant` runs on the node `other`, not on this \
                       node `edge`";
        let fix = "Run it with `--data` of node `other`";
        assert_eq!(reply, refusal("ops.browse-node", 2, message, fix));

        let edge = site("tree", "edge", "");
        let reply = served(&node, "@admin", &at(&edge, &query)).await;
        let message = "the start `a//b` is not a path: a name in it is empty";
        let fix = "Give folder names joined by `/`, with a `\\` before each `/`, `*`, \
                   or `\\` in a name";
        assert_eq!(reply, refusal("ops.bad-start", 2, message, fix));

        let query = json!({ "start": "Line3", "match": "a\\", "cursor": "0" });
        let reply = served(&node, "@admin", &at(&edge, &query)).await;
        let message = "the match `a\\` is not a pattern: it ends in a `\\` that takes \
                       no character";
        let fix = "Give names joined by `/`, at most 64 other than `**` and at most \
                   256 `*`, with a `\\` before each `/`, `*`, or `\\` in a name";
        assert_eq!(reply, refusal("ops.bad-pattern", 2, message, fix));

        let query = json!({ "start": "Line3", "match": "*", "cursor": "0" });
        let reply = served(&node, "@admin", &at(&edge, &query)).await;
        let message = "a browse takes `match` or `cursor`, not both";
        assert_eq!(reply, refusal("ops.argument", 2, message, fix_of(&reply)));

        let query = json!({ "start": "Line3", "cursor": "4" });
        let reply = served(&node, "@admin", &at(&edge, &query)).await;
        assert_eq!(reply["output"]["end"], "done", "{reply}");
    });
}

/// A browse needs `admin` too, as a remote `plan` needs it to read the sources.
#[test]
fn needs_apply_and_admin_on_the_name_of_the_connector() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let people = format!(
            "subject \"eve\" {{ keys = [\"{EVE}\"] }}\naccess \"browser\" {{\n  \
             subjects = \"eve\"\n  select = \"plant\"\n  allow = [apply]\n}}\n"
        );
        applied(&node, &people).await;
        let edge = site("tree", "edge", "");
        let request = body("plant", "site.hcl", &edge, &json!({}));
        let reply = served(&node, "eve", &request).await;
        let message = "no access policy gives `eve` the action `admin` on `plant`";
        let fix = "Ask an admin of the region for an access policy that allows it";
        assert_eq!(reply, refusal("ops.denied", 1, message, fix));

        applied(&node, &people.replace("[apply]", "[apply, admin]")).await;
        let reply = served(&node, "eve", &request).await;
        assert_eq!(reply["output"], browsed(&node, "tree", &json!({})).await);
    });
}

/// The fix of the one problem of `reply`.
fn fix_of(reply: &Value) -> &str {
    reply["errors"][0]["fix"].as_str().expect("a fix")
}

#[test]
fn a_bad_start_or_a_match_with_too_many_names_or_stars_gives_its_cause() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let edge = site("tree", "edge", "");
        let deep = vec!["a"; 65].join("/");
        let stars = "*a".repeat(257);
        let cases = [
            (
                json!({ "start": "a\\" }),
                "ops.bad-start",
                "the start `a\\` is not a path: it ends in a `\\` that takes no \
                 character"
                    .to_owned(),
            ),
            (
                json!({ "match": deep }),
                "ops.bad-pattern",
                format!(
                    "the match `{deep}` is not a pattern: it has more than 64 names, \
                     and a browse finds no path that long"
                ),
            ),
            (
                json!({ "match": stars }),
                "ops.bad-pattern",
                format!(
                    "the match `{stars}` is not a pattern: it has more than 256 `*`, \
                     and each costs a search of each name"
                ),
            ),
        ];
        for (query, code, message) in cases {
            let reply =
                served(&node, "@admin", &body("plant", "site.hcl", &edge, &query))
                    .await;
            assert_eq!(reply["errors"][0]["code"], code, "{reply}");
            assert_eq!(reply["errors"][0]["message"], message);
        }
    });
}

/// A browse and a plan of a file of 70001 blocks of one name, whose 70000 problems do
/// not fit in one reply, give the problems that fit, then `ops.more-problems`.
#[test]
fn a_file_of_more_problems_than_a_reply_holds_gives_those_that_fit() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let text = site("tree", "edge", "").repeat(70_001);
        let browse = body("plant", "site.hcl", &text, &json!({}));
        let reply = serve(&node, &name("@admin"), &browse).await;
        let browsed = more_problems(&reply, "config.duplicate-name", 70_000, 2);
        let plan =
            json!({ "plan": { "files": [{ "path": "site.hcl", "text": text }] } });
        let reply = serve(&node, &name("@admin"), format!("{plan}\n").as_bytes()).await;
        assert_eq!(
            browsed,
            more_problems(&reply, "config.duplicate-name", 70_000, 2)
        );
    });
}

/// The broken kind gives a page over the most, which the walk refuses.
/// A start whose error passes the bound of a reply, in a request inside the bound,
/// gives the fix of a browse, not the fix of a plan.
#[test]
fn a_start_whose_error_passes_the_bound_of_a_reply_gives_a_fix_of_a_browse() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let site = site("tree", "edge", "");
        let empty = body("plant", "site.hcl", &site, &json!({ "start": "" }));
        let max = usize::try_from(BODY_BYTES_MAX).expect("a usize");
        let start = format!("{}//", "a".repeat(max - empty.len() - 2));
        let request = body("plant", "site.hcl", &site, &json!({ "start": start }));
        assert_eq!(request.len(), max);
        assert_eq!(served(&node, "@admin", &request).await, large_browse());
    });
}

#[test]
fn the_command_line_writes_the_defect_of_a_kind_that_breaks_a_rule_of_a_browse() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let paths = host(&fixture, "broken");
        let shown = cli(
            &node,
            &paths,
            &["browse", "plant", "site.hcl", "--start", "Line3"],
        )
        .await;
        let errors = "error[connector.broken-browse]: the browse of the connector \
                      `plant` of kind `broken` failed: the connector kind broke a rule \
                      of a browse: a page holds at most 500 entries, and a page of \
                      \"Line3\" held 501\n\
                      fix: Report this defect of the connector kind with this \
                      message\n";
        assert_eq!(shown, (Run::Exit(1), String::new(), errors.to_owned()));
    });
}

#[test]
fn a_failure_of_the_device_gives_browse_with_its_cause() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let edge = site("failing", "edge", "");
        let reply = served(
            &node,
            "@admin",
            &body("plant", "site.hcl", &edge, &json!({})),
        )
        .await;
        let message = failed(
            "failing",
            "the device is in a bad state: the server closed the session",
        );
        let fix = "Fix the cause in the message, then browse again";
        assert_eq!(reply, refusal("ops.browse", 1, &message, fix));
    });
}

#[test]
fn a_retry_gives_browse_with_its_cause() {
    let cause = connector::kind::Error::Retry("the request timed out".into());
    let error =
        Error::browse(&name("plant"), &name("tree"), cause, &Sources::default());
    let message = failed("tree", "an attempt failed: the request timed out");
    let mut expected = refusal(
        "ops.browse",
        1,
        &message,
        "Fix the cause in the message, then browse again",
    );
    expected
        .as_object_mut()
        .expect("an object")
        .remove("status");
    assert_eq!((error.json(), error.status()), (expected, 1));
    assert_eq!(error.to_string(), message);
}

/// The text of an error of many problems counts them, with the connector and the kind.
#[test]
fn an_error_of_many_problems_counts_them() {
    let problem = |message: &str| {
        Diagnostic::new(common::ADDRESS, None, message.to_owned(), "Fix".to_owned())
    };
    let cause = connector::kind::Error::Config(vec![problem("a"), problem("b")]);
    let error =
        Error::browse(&name("plant"), &name("tree"), cause, &Sources::default());
    assert_eq!(
        error.to_string(),
        "the browse of the connector `plant` of kind `tree` failed with 2 problems"
    );
}

#[test]
fn refuses_bytes_after_the_first_line() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let site = site("tree", "edge", "");
        let mut request = body("plant", "site.hcl", &site, &json!({}));
        request.extend_from_slice(b"more");
        let reply = served(&node, "@admin", &request).await;
        let message = "the body is no request: bytes follow the first line of `browse`";
        assert_eq!(reply["errors"][0]["code"], "ops.bad-request", "{reply}");
        assert_eq!(reply["errors"][0]["message"], message, "{reply}");
    });
}

/// A problem of the config of the kind keeps its code, fix, place, and notes.
#[test]
fn a_problem_of_the_config_keeps_its_code_fix_place_and_notes() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let text = site("tree", "edge", "  address = \"bad\"\n");
        let reply = served(
            &node,
            "@admin",
            &body("plant", "site.hcl", &text, &json!({})),
        )
        .await;
        let expected = json!({
            "errors": [{
                "code": common::ADDRESS.as_str(),
                "message": failed("tree", "the address \"bad\" names no device"),
                "fix": "Give the address of a device",
                "place": { "file": "site.hcl", "line": 4, "column": 13 },
                "notes": [{
                    "text": "the key of the address",
                    "place": { "file": "site.hcl", "line": 4, "column": 3 },
                }],
            }],
            "status": 1,
        });
        assert_eq!(reply, expected);
    });
}

/// A level gives a page with each shape and a cursor, and the cursor gives the next
/// page. A fact that the device cannot know is `null`, and `?` in the text.
#[test]
fn a_level_gives_each_shape_and_its_next_page_at_the_cursor() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let first = browsed(&node, "tree", &json!({ "start": "Line3" })).await;
        let expected = json!({
            "entries": [
                { "path": "Line3/Cell", "tag": "line3.cell", "shape": "folder" },
                point("Line3/Temperature", "line3.temperature", &json!({
                    "type": "f64", "unit": "°C", "unit_code": "CEL", "access": "read",
                })),
                point("Line3/Bare", "line3.bare", &unknown()),
                point("Line3/Heater", "line3.heater", &json!({
                    "type": "u16", "unit": null, "unit_code": "KWT", "access": "write",
                })),
            ],
            "end": { "next": "4" },
        });
        assert_eq!(first, expected);
        let lines = "Line3/Cell/\n\
                     Line3/Temperature f64 °C read\n\
                     Line3/Bare ? ? ?\n\
                     Line3/Heater u16 KWT write\n\
                     more: --cursor 4\n";
        assert_eq!(text(&first), lines);

        let query = json!({ "start": "Line3", "cursor": "4" });
        let next = browsed(&node, "tree", &query).await;
        let expected = json!({
            "entries": [
                {
                    "path": "Line3/Recipe",
                    "tag": "line3.recipe",
                    "shape": "unreadable",
                    "reason": "an array",
                },
                point("Line3/Setpoint", "line3.setpoint", &json!({
                    "type": "f32",
                    "unit": "%",
                    "unit_code": "P1",
                    "access": "read-write",
                })),
            ],
            "end": "done",
        });
        assert_eq!(next, expected);
        let lines =
            "Line3/Recipe unreadable: an array\nLine3/Setpoint f32 % read-write\n";
        assert_eq!(text(&next), lines);

        let root = browsed(&node, "tree", &json!({})).await;
        let paths: Vec<_> = root["entries"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|e| e["path"].clone())
            .collect();
        assert_eq!(paths, ["Line3", "Types", "Many", "Huge"]);
        assert_eq!(root["end"], "done");
    });
}

/// With values, each point has a `value`, `null` when the device gave none, and a
/// folder or an unreadable point has none.
#[test]
fn values_are_only_on_points_and_null_when_the_device_gave_none() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let query = json!({ "start": "Line3", "values": true });
        let first = browsed(&node, "tree", &query).await;
        let entries = first["entries"].as_array().expect("a list");
        assert_eq!(entries[0].get("value"), None);
        assert_eq!(entries[1]["value"], 21.5);
        let mut bare = unknown();
        bare["value"] = Value::Null;
        assert_eq!(entries[2], point("Line3/Bare", "line3.bare", &bare));
        assert_eq!(entries[3]["value"], 3);
        let lines = "Line3/Cell/\n\
                     Line3/Temperature f64 °C read 21.5\n\
                     Line3/Bare ? ? ? ?\n\
                     Line3/Heater u16 KWT write 3\n\
                     more: --cursor 4\n";
        assert_eq!(text(&first), lines);
        let query = json!({ "start": "Line3", "cursor": "4", "values": true });
        let next = browsed(&node, "tree", &query).await;
        assert_eq!(next["entries"][0].get("value"), None);
        assert_eq!(next["entries"][1]["value"], 42.0);
    });
}

/// A match with values gives each value type in its JSON form.
#[test]
fn a_match_with_values_gives_each_value_type() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let query = json!({ "start": "Types", "match": "*", "values": true });
        let found = browsed(&node, "tree", &query).await;
        let values: Vec<_> = found["entries"]
            .as_array()
            .expect("a list")
            .iter()
            .map(|entry| (entry["type"].clone(), entry["value"].clone()))
            .collect();
        let expected = [
            (json!("bool"), json!(true)),
            (json!("i8"), json!(-8)),
            (json!("i16"), json!(-16)),
            (json!("i32"), json!(-32)),
            (json!("i64"), json!(i64::MIN.to_string())),
            (json!("u8"), json!(8)),
            (json!("u16"), json!(16)),
            (json!("u32"), json!(32)),
            (json!("u64"), json!(u64::MAX.to_string())),
            (json!("f32"), json!(0.1)),
            (json!("f64"), json!(-2.5)),
            (json!("f64"), json!("NaN")),
            (json!("f32"), json!("Infinity")),
            (json!("f64"), json!("-Infinity")),
            (json!("timestamp"), json!("2023-11-14T22:13:20.000000001Z")),
            (json!("duration"), json!("250us")),
            (json!("uuid"), json!("01234567-89ab-cdef-0123-456789abcdef")),
            (Value::Null, Value::Null),
        ];
        assert_eq!(values, expected);
        assert_eq!(found["end"], "done");
        let shown = text(&found);
        let lines: Vec<_> = shown.lines().collect();
        assert_eq!(lines[0], "Types/bool bool ? read true");
        assert_eq!(lines[9], "Types/f32 f32 ? read 0.1");
        assert_eq!(lines[11], "Types/nan f64 ? read NaN");
        assert_eq!(
            lines[16],
            "Types/uuid uuid ? read 01234567-89ab-cdef-0123-456789abcdef"
        );
        assert_eq!(lines[17], "Types/none ? ? ? ?");
        assert_eq!(lines.len(), 18);
    });
}

/// An integer of 64 bits is its decimal text, also past 2^53, where a JSON number loses
/// digits in JavaScript, and a float that is not finite is the text of proto3 JSON.
#[test]
fn a_value_is_json_as_proto3_gives_it() {
    let cases = [
        (
            sample::Value::U64(9_007_199_254_740_993),
            json!("9007199254740993"),
        ),
        (
            sample::Value::I64(-9_007_199_254_740_993),
            json!("-9007199254740993"),
        ),
        (sample::Value::U32(u32::MAX), json!(4_294_967_295_u32)),
        (sample::Value::I32(i32::MIN), json!(-2_147_483_648_i32)),
        (sample::Value::F32(f32::NAN), json!("NaN")),
        (sample::Value::F32(f32::INFINITY), json!("Infinity")),
        (sample::Value::F32(f32::NEG_INFINITY), json!("-Infinity")),
        (sample::Value::F64(f64::NAN), json!("NaN")),
        (sample::Value::F64(f64::INFINITY), json!("Infinity")),
        (sample::Value::F64(f64::NEG_INFINITY), json!("-Infinity")),
    ];
    for (value, expected) in cases {
        let shown = format!("{value:?}");
        let json = serde_json::to_value(super::Value::of(value)).expect("JSON");
        assert_eq!(json, expected, "{shown}");
    }
}

/// A match of more points than the most gives `limit`, and a match that looks at more
/// entries than the budget gives `budget`.
#[test]
fn a_match_ends_at_the_limit_or_at_the_budget() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let found =
            browsed(&node, "tree", &json!({ "start": "Many", "match": "*" })).await;
        assert_eq!(found["entries"].as_array().expect("a list").len(), 500);
        assert_eq!(found["end"], "limit");
        let shown = text(&found);
        assert_eq!(
            shown.lines().last(),
            Some(
                "More than 500 points match, the most for one browse; narrow the \
                 pattern or the start"
            )
        );

        let query = json!({ "start": "Huge", "match": "none" });
        let found = browsed(&node, "tree", &query).await;
        assert_eq!(found, json!({ "entries": [], "end": "budget" }));
        let lines = "The browse found no entries.\n\
                     The browse stopped at its budget, so more may match; narrow the \
                     pattern or the start\n";
        assert_eq!(text(&found), lines);
    });
}

/// A browse that stops at its deadline gives `deadline`, and its text names it.
#[test]
fn a_browse_at_the_deadline_says_so() {
    let found = connector::browse::Found {
        entries: Vec::new(),
        values: Vec::new(),
        end: connector::browse::End::Deadline,
    };
    let output = serde_json::to_value(Output::of(found)).expect("JSON");
    assert_eq!(output, json!({ "entries": [], "end": "deadline" }));
    let lines = "The browse found no entries.\n\
                 The browse stopped after 1m, so more may match; browse a deeper \
                 start\n";
    assert_eq!(text(&output), lines);
}

/// The command line writes the text of a page, and with `--json` its JSON.
#[test]
fn the_command_line_writes_a_page_as_text_or_json() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let paths = host(&fixture, "tree");
        let args = [
            "browse", "plant", "site.hcl", "--start", "Line3", "--cursor", "4",
        ];
        let shown = cli(&node, &paths, &args).await;
        let lines =
            "Line3/Recipe unreadable: an array\nLine3/Setpoint f32 % read-write\n";
        assert_eq!(shown, (Run::Exit(0), lines.to_owned(), String::new()));
        let (run, json, errors) =
            cli(&node, &paths, &[&args[..], &["--json"]].concat()).await;
        assert_eq!((run, errors.as_str()), (Run::Exit(0), ""));
        let query = json!({ "start": "Line3", "cursor": "4" });
        assert_eq!(json, format!("{}\n", browsed(&node, "tree", &query).await));
        let args = [
            "browse", "plant", "site.hcl", "--match", "*", "--cursor", "4",
        ];
        let (run, output, errors) = cli(&node, &paths, &args).await;
        assert_eq!((run, output.as_str()), (Run::Exit(2), ""));
        assert!(errors.starts_with("error[ops.argument]: "), "{errors}");
    });
}

/// The command line browses the connector of a file in a directory, and reads only
/// its config files.
#[test]
fn the_command_line_browses_a_connector_in_a_directory() {
    solo(|fixture, mesh| async move {
        let node = create_browsing_node(&fixture, mesh);
        let paths = host(&fixture, "tree");
        fixture.sim.make_dir(Path::new("/work/cfg"));
        let site = site("tree", "edge", "");
        let files = [
            ("cfg/a.hcl", ""),
            ("cfg/site.hcl", site.as_str()),
            ("cfg/README.md", "not HCL"),
        ];
        for (path, text) in files {
            paths
                .write(Path::new(path), text.as_bytes())
                .expect("a write");
        }
        let args = [
            "browse", "plant", "cfg", "--start", "Line3", "--cursor", "4",
        ];
        let shown = cli(&node, &paths, &args).await;
        let lines =
            "Line3/Recipe unreadable: an array\nLine3/Setpoint f32 % read-write\n";
        assert_eq!(shown, (Run::Exit(0), lines.to_owned(), String::new()));
    });
}

/// Tests with kinds of their own.
mod own_kinds {
    use std::path::Path;

    use connector::browse::{self, End, Found, Query};
    use connector::kind::{self, Channels, Context, Table};
    use document::Document;
    use document::diagnostic::Diagnostic;
    use mesh::Mesh;
    use serde_json::json;
    use types::sample;
    use wire::hub::client::BODY_BYTES_MAX;

    use crate::Node;
    use crate::common::{
        BOB, Fixture, create_config, create_supervisor, front_ends, solo,
    };

    /// The kind `own` of one test, by what its browse does.
    enum Own {
        /// Reads the secret of its `token`, as a kind reads the password of its
        /// device.
        Probe,
        /// Gives a root of one `f64` point with this value.
        One(f64),
        /// Gives a root of one page of 500 points, each with a name of 40 000 bytes: a
        /// page that `walk` takes, whose JSON passes the bound of a reply.
        Wide,
        /// Fails with a device error of `BODY_BYTES_MAX` bytes.
        Loud,
    }

    impl kind::Kind for Own {
        type Config = ();
        const SCHEMA: document::schema::Body = connector::testing::READS;

        fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
            Ok(())
        }

        fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
            Ok(Channels::default())
        }

        fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), kind::Error>> {
            std::future::ready(Ok(()))
        }

        async fn browse(
            &self,
            ctx: &browse::Context<'_>,
            query: &Query,
        ) -> Result<Found, kind::Error> {
            match self {
                Self::Probe => {
                    let token = &ctx.config().attributes.get("token").expect("a token");
                    let source = secret::Source::read(&token.value).expect("a source");
                    ctx.secret(&source).await?;
                    Ok(Found {
                        entries: Vec::new(),
                        values: Vec::new(),
                        end: End::Done,
                    })
                }
                Self::One(x) => {
                    browse::walk(&mut OneSession(*x), query, ctx.clock()).await
                }
                Self::Wide => browse::walk(&mut WideSession, query, ctx.clock()).await,
                Self::Loud => {
                    let len = usize::try_from(BODY_BYTES_MAX).expect("a usize");
                    Err(kind::Error::Device("x".repeat(len).into()))
                }
            }
        }
    }

    struct OneSession(f64);

    impl browse::Device for OneSession {
        fn level(
            &mut self,
            folder: Option<&browse::Entry>,
            _: Option<&browse::Cursor>,
        ) -> impl Future<Output = Result<Option<browse::Page>, kind::Error>> {
            let page = folder.is_none().then(|| browse::Page {
                entries: vec![browse::Child {
                    name: "x".to_owned(),
                    tag: "x".to_owned(),
                    shape: browse::Shape::Point(browse::Facts::default()),
                }],
                next: None,
            });
            std::future::ready(Ok(page))
        }

        fn values(
            &mut self,
            tags: &[&str],
        ) -> impl Future<Output = Result<Vec<Option<sample::Value>>, kind::Error>>
        {
            let values = tags
                .iter()
                .map(|_| Some(sample::Value::F64(self.0)))
                .collect();
            std::future::ready(Ok(values))
        }
    }

    struct WideSession;

    impl browse::Device for WideSession {
        fn level(
            &mut self,
            folder: Option<&browse::Entry>,
            _: Option<&browse::Cursor>,
        ) -> impl Future<Output = Result<Option<browse::Page>, kind::Error>> {
            let page = folder.is_none().then(|| browse::Page {
                entries: (0..500)
                    .map(|n| browse::Child {
                        name: format!("p{n}{}", "a".repeat(40_000)),
                        tag: format!("t{n}"),
                        shape: browse::Shape::Point(browse::Facts::default()),
                    })
                    .collect(),
                next: None,
            });
            std::future::ready(Ok(page))
        }
    }

    /// A node whose kind `own` is `own`.
    fn create_own_node(fixture: &Fixture, mesh: Mesh, own: Own) -> Node {
        let secrets = connector::testing::create_secrets(fixture.sim.files());
        Node::new(crate::node::Config {
            supervisor: create_supervisor(
                fixture,
                Table::new().with("own", own),
                secrets.clone(),
            ),
            ..create_config(fixture, mesh, front_ends(), secrets)
        })
    }

    /// A finite `f32` is the shortest text that reads back as it, at the node and
    /// again after the command line parses the reply.
    #[test]
    fn an_f32_is_its_shortest_text_at_each_exponent() {
        use super::super::Value as Json;
        let mut wrong = Vec::new();
        for exponent in 0..255_u32 {
            for mantissa in [0, 1, 0x12_3456, 0x40_0000, 0x7f_ffff] {
                for sign in [0, 1 << 31] {
                    let x = f32::from_bits(sign | exponent << 23 | mantissa);
                    // The text of the shortest decimal that reads back as `x`.
                    let shortest = serde_json::to_string(&x).expect("JSON");
                    let denoted =
                        |text: &str| text.parse::<f64>().expect("a number").to_bits();
                    let at_node =
                        serde_json::to_string(&Json::of(sample::Value::F32(x)))
                            .expect("JSON");
                    let parsed: Json = serde_json::from_str(&at_node).expect("a value");
                    let at_cli = serde_json::to_string(&parsed).expect("JSON");
                    if denoted(&at_node) != denoted(&shortest) || at_cli != at_node {
                        wrong.push((shortest, at_node, at_cli));
                    }
                }
            }
        }
        assert_eq!(wrong, Vec::<(String, String, String)>::new());
    }

    /// `foundation browse --values --json` gives the `f64` that the device gave.
    #[test]
    fn the_command_line_gives_the_f64_value_of_the_device() {
        solo(|fixture, mesh| async move {
            let x = 1.031_111_111_117_692_3_f64;
            let node = create_own_node(&fixture, mesh, Own::One(x));
            let paths = super::host(&fixture, "own");
            let args = ["browse", "plant", "site.hcl", "--values", "--json"];
            let (run, json, errors) = super::cli(&node, &paths, &args).await;
            assert_eq!((run, errors.as_str()), (crate::Run::Exit(0), ""));
            // The text, not a parse of it: a parse of serde_json can round wrong too.
            let value = format!("\"value\":{x}}}");
            assert_eq!(x.to_string(), "1.0311111111176923");
            assert!(json.contains(&value), "{json} does not hold {value}");
        });
    }

    /// The command line gives each `f64` that the node gave, bit for bit.
    #[test]
    fn the_command_line_gives_the_f64_of_the_node() {
        use super::super::Value as Json;
        let mut wrong = Vec::new();
        let mut bits: u64 = 0x0010_0000_0000_0000;
        while bits < 0x7ff0_0000_0000_0000 {
            let x = f64::from_bits(bits);
            let at_node =
                serde_json::to_string(&Json::of(sample::Value::F64(x))).expect("JSON");
            let parsed: Json = serde_json::from_str(&at_node).expect("a value");
            let at_cli = serde_json::to_string(&parsed).expect("JSON");
            if at_cli != at_node {
                wrong.push((at_node, at_cli));
            }
            bits += 0x0000_1234_5678_9abd;
        }
        let count = wrong.len();
        wrong.truncate(3);
        assert_eq!((count, wrong), (0, Vec::new()));
    }

    /// The body of a `browse` of `site.c` in `c.hcl` of `text`.
    fn browse_body(text: &str) -> Vec<u8> {
        super::body("site.c", "c.hcl", text, &json!({}))
    }

    /// The text of `c.hcl` with `site.c` of the kind `own` on `edge`, and `more` lines
    /// in its block.
    fn own_site(more: &str) -> String {
        format!(
            "connector \"site.c\" {{\n  kind = \"own\"\n  node = \"edge\"\n{more}}}\n"
        )
    }

    /// Asserts that a browse of `own` gives `ops.large-browse`.
    fn gives_large_browse(own: Own) {
        solo(|fixture, mesh| async move {
            let node = create_own_node(&fixture, mesh, own);
            let body = browse_body(&own_site(""));
            assert_eq!(
                super::served(&node, "@admin", &body).await,
                super::large_browse()
            );
        });
    }

    /// A level whose page passes the bound of a reply gives the fix of a browse, not
    /// the fix of a plan.
    #[test]
    fn a_level_past_the_bound_of_a_reply_gives_a_fix_of_a_browse() {
        gives_large_browse(Own::Wide);
    }

    /// A device error that passes the bound of a reply gives the fix of a browse, not
    /// the fix of a plan.
    #[test]
    fn a_device_error_past_the_bound_of_a_reply_gives_a_fix_of_a_browse() {
        gives_large_browse(Own::Loud);
    }

    /// `bob` has `apply` and no `admin` on `site.c`, so a browse reads no source for
    /// him, as a remote `plan` does not.
    #[test]
    fn a_subject_with_apply_and_no_admin_learns_nothing_of_the_files_of_the_node() {
        solo(|fixture, mesh| async move {
            let node = create_own_node(&fixture, mesh, Own::Probe);
            let people = format!(
                "subject \"bob\" {{ keys = [\"{BOB}\"] }}\n\
                 access \"planner\" {{\n  subjects = \"bob\"\n  select = \"**\"\n  \
                 allow = [plan]\n}}\n\
                 access \"site-apply\" {{\n  subjects = \"bob\"\n  select = \
                 \"site.**\"\n  allow = [apply]\n}}\n"
            );
            super::applied(&node, &people).await;

            let paths = fixture.sim.paths();
            fixture.sim.make_dir(Path::new("/run"));
            paths
                .write(Path::new("/run/there"), b"x")
                .expect("writes it");
            let probe = |path: &str| own_site(&format!("  token = file(\"{path}\")\n"));
            let seen =
                super::served(&node, "bob", &browse_body(&probe("/run/there"))).await;
            let unseen =
                super::served(&node, "bob", &browse_body(&probe("/run/absent"))).await;
            let message = "no access policy gives `bob` the action `admin` on `site.c`";
            let fix = "Ask an admin of the region for an access policy that allows it";
            let denied = super::refusal("ops.denied", 1, message, fix);
            assert_eq!((seen, unseen), (denied.clone(), denied));
        });
    }
}

#[test]
fn the_text_of_no_entries_says_so() {
    let output = Output {
        entries: Vec::new(),
        end: super::End::Done,
    };
    assert_eq!(output.text(), "The browse found no entries.\n");
}

#[test]
fn the_text_of_a_failed_browse_escapes_only_control_characters() {
    let cause = connector::kind::Error::Device("a\\b\u{1b}[2J".into());
    let error =
        Error::browse(&name("plant"), &name("tree"), cause, &Sources::default());
    let message = failed("tree", r"the device is in a bad state: a\b\u{1b}[2J");
    assert_eq!(
        error.text(),
        format!(
            "error[ops.browse]: {message}\n\
             fix: Fix the cause in the message, then browse again\n"
        )
    );
}

#[test]
fn the_text_of_a_page_escapes_control_characters() {
    let point = super::Point {
        data_type: Some("f64".to_owned()),
        unit: Some("\u{1b}[2J".to_owned()),
        unit_code: None,
        access: Some(super::Access::Read),
        value: None,
    };
    let output = Output {
        entries: vec![
            super::Entry {
                path: "a\u{1b}b".to_owned(),
                tag: "t".to_owned(),
                shape: super::Shape::Folder,
            },
            super::Entry {
                path: "a\\c".to_owned(),
                tag: "u".to_owned(),
                shape: super::Shape::Point(point),
            },
            super::Entry {
                path: "d".to_owned(),
                tag: "v".to_owned(),
                shape: super::Shape::Unreadable {
                    reason: "an\narray".to_owned(),
                },
            },
        ],
        end: super::End::Next("4\u{7}".to_owned()),
    };
    assert_eq!(
        output.text(),
        "a\\u{1b}b/\n\
         a\\c f64 \\u{1b}[2J read\n\
         d unreadable: an\\narray\n\
         more: --cursor 4\\u{7}\n"
    );
}
