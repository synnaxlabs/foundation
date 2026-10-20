use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use mesh::used::Cause;
use serde_json::{Value, json};
use spec::definition::Definition;
use types::name::Name;
use wire::hub::client::BODY_BYTES_MAX;

use super::{Answer, Body, Large, failed, serve};
use crate::Node;
use crate::common::{
    BOB, EVE, NODE, SAM, SITE, create_node, fail_sync, more_problems, name,
    placed_site, plant, solo,
};
use crate::error::{Error, Problem};
use crate::front_end::File;
use crate::plan;

const DENIED_FIX: &str =
    "Ask an admin of the region for an access policy that allows it";
/// What `ops.denied` names for a request that names no definition.
const NOTHING: &str = "a request that names no definition";

/// The first line of the reply of `ops.large-reply`.
fn large() -> Value {
    let message = format!(
        "the reply has more than {BODY_BYTES_MAX} bytes, the most that a reply holds"
    );
    refusal(
        "ops.large-reply",
        1,
        &message,
        "Plan and apply the change in smaller steps",
    )
}

/// The subject of the first admin, which may do each action on each name.
fn admin() -> Name {
    name("@admin")
}

/// Three subjects. `bob` may plan each name. `sam` may plan and apply each name
/// under `site`. `eve` has no access policy.
fn people() -> String {
    format!(
        r#"subject "bob" {{ keys = ["{BOB}"] }}
subject "eve" {{ keys = ["{EVE}"] }}
subject "sam" {{ keys = ["{SAM}"] }}
access "planner" {{
  subjects = "bob"
  select = "**"
  allow = [plan]
}}
access "surveyor" {{
  subjects = "sam"
  select = "site.**"
  allow = [plan, apply]
}}
"#
    )
}

/// `site.hcl` with a placement under `site` that homes its index on `edge`.
fn site() -> String {
    let placement = "placement \"site.home\" {\n  select = \"site.*\"\n  home = \
                     \"edge\"\n}\n";
    format!("{SITE}{placement}")
}

/// The first line of the reply of `ops.stale-plan` for a plan at `base` on a region
/// at `pointer`.
fn stale(base: spec::Pointer, pointer: spec::Pointer) -> Value {
    let message = format!(
        "the spec changed: the spec is at {pointer}, not at the base {base} of the plan"
    );
    refusal("ops.stale-plan", 1, &message, "Plan again")
}

/// The body of a `plan` request of `files`, each a path and its text.
fn plan_body(files: &[(&str, &str)]) -> Vec<u8> {
    let files: Vec<Value> = files
        .iter()
        .map(|(path, text)| json!({ "path": path, "text": text }))
        .collect();
    format!("{}\n", json!({ "plan": { "files": files } })).into_bytes()
}

/// The body of an `apply` request of the plan file `plan`.
fn apply_body(plan: &[u8]) -> Vec<u8> {
    [b"{\"apply\":{}}\n", plan].concat()
}

/// The first line of `reply`, as JSON, and the bytes after it.
fn split(reply: &[u8]) -> (Value, &[u8]) {
    let end = reply
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("a reply has a first line");
    let head = serde_json::from_slice(&reply[..end]).expect("a first line of JSON");
    (head, &reply[end + 1..])
}

/// The first line of the reply of one error with no place.
fn refusal(code: &str, status: u8, message: &str, fix: &str) -> Value {
    json!({
        "errors": [{ "code": code, "message": message, "fix": fix, "notes": [] }],
        "status": status,
    })
}

/// The first line of the reply of `ops.bad-request` with `message`.
fn bad(message: &str) -> Value {
    refusal(
        "ops.bad-request",
        1,
        &format!("the body is no request: {message}"),
        "Use a `foundation` program of the same version as the node",
    )
}

/// The first line of the reply of `ops.denied` for `subject` and `action` on `on`.
fn denied(subject: &str, action: &str, on: &str) -> Value {
    let message =
        format!("no access policy gives `{subject}` the action `{action}` on {on}");
    refusal("ops.denied", 1, &message, DENIED_FIX)
}

/// The plan file of `files` on `node`, as `@admin` plans it through `serve`.
async fn planned(node: &Node, files: &[(&str, &str)]) -> Vec<u8> {
    let reply = serve(node, &admin(), &plan_body(files)).await;
    let (head, plan) = split(&reply);
    assert!(head.get("output").is_some(), "{head}");
    plan.to_vec()
}

/// Applies `files` on `node` as `@admin`, through `serve`.
async fn applied(node: &Node, files: &[(&str, &str)]) {
    let plan = planned(node, files).await;
    let reply = serve(node, &admin(), &apply_body(&plan)).await;
    let (head, rest) = split(&reply);
    assert!(head.get("output").is_some(), "{head}");
    assert_eq!(rest, b"");
}

#[test]
fn plans_applies_and_plans_no_change() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let site = placed_site();
        let files = vec![(PathBuf::from("site.hcl"), site.clone())];
        let (plan, output) = node.plan(files.clone()).await.expect("a plan");
        assert_eq!(output["added"], 3, "{output}");
        let reserved = json!([
            { "name": "@admin", "kind": "access", "key": "@admin.@access" },
            { "name": "@admin", "kind": "subject", "key": "@admin.@subject" },
        ]);
        assert_eq!(output["reserved"], reserved);
        let body = plan_body(&[("site.hcl", &site)]);
        let reply = serve(&node, &admin(), &body).await;
        assert_eq!(split(&reply), (json!({ "output": output }), &*plan));

        let reply = serve(&node, &admin(), &apply_body(&plan)).await;
        let pointer = plan::Pointer::from(mesh.pointer());
        assert_eq!(pointer.version, 1);
        let effect = format!(
            "{{\"output\":{{\"pointer\":{{\"version\":1,\"root\":\"{}\"}},\
             \"added\":3,\"changed\":0,\"removed\":0,\"homes\":1}}}}\n",
            pointer.root
        );
        assert_eq!(String::from_utf8(reply), Ok(effect));

        let (plan, output) = node.plan(files).await.expect("a second plan");
        assert_eq!(output["changes"], json!([]), "{output}");
        let reply = serve(&node, &admin(), &body).await;
        assert_eq!(split(&reply), (json!({ "output": output }), &*plan));
        let reply = serve(&node, &admin(), &apply_body(&plan)).await;
        let (head, rest) = split(&reply);
        assert_eq!(head["output"]["pointer"]["version"], 1, "{head}");
        assert_eq!(rest, b"");
    });
}

/// A config problem whose message is `len` times `a`.
fn problem(len: usize) -> Problem {
    Problem {
        code: "config.duplicate-name".to_owned(),
        message: "a".repeat(len),
        fix: String::new(),
        place: None,
        notes: Vec::new(),
    }
}

/// The reply of `errors` with status 2.
fn reply_of(errors: &[Problem]) -> Vec<u8> {
    let failed = Answer::<(), _>::Failed { errors, status: 2 };
    let mut reply = serde_json::to_vec(&failed).expect("JSON");
    reply.push(b'\n');
    reply
}

#[test]
fn an_error_over_the_bound_keeps_the_problems_that_fit_to_the_byte() {
    let max = usize::try_from(BODY_BYTES_MAX).expect("a usize");
    let rest = [problem(200), problem(200)];
    let empty = reply_of(&[problem(0), Problem::more(rest.len())]).len();
    let fitting = [problem(max - empty)].into_iter().chain(rest.clone());
    let error = Error::Config(fitting.collect());
    let reply = failed(&error, &Error::LargeReply).0;
    assert_eq!(reply.len(), max);
    assert_eq!(reply, reply_of(&[problem(max - empty), Problem::more(2)]));
    let over = [problem(max - empty + 1)].into_iter().chain(rest);
    let reply = failed(&Error::Config(over.collect()), &Error::LargeReply).0;
    assert_eq!(split(&reply), (large(), &b""[..]));
}

/// The count of the rest has one digit fewer after the second problem than after the
/// first, so the second fits only when its own count is used.
#[test]
fn an_error_over_the_bound_counts_the_rest_after_each_problem() {
    let max = usize::try_from(BODY_BYTES_MAX).expect("a usize");
    let rest = std::iter::repeat_n(problem(200), 9);
    let empty = reply_of(&[problem(0), problem(0), Problem::more(9)]).len();
    let kept = [problem(max - empty), problem(0)];
    let error = Error::Config(kept.iter().cloned().chain(rest).collect());
    let reply = failed(&error, &Error::LargeReply).0;
    assert_eq!(reply.len(), max);
    assert_eq!(
        reply,
        reply_of(&[kept[0].clone(), kept[1].clone(), Problem::more(9)])
    );
}

#[test]
fn a_plan_error_whose_first_problem_does_not_fit_gives_large_reply() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh);
        let max = usize::try_from(BODY_BYTES_MAX).expect("a usize");
        let path = format!("{}.hcl", "x".repeat(max - 200));
        let body = plan_body(&[(&path, "nope \"a\" {}\n")]);
        assert!(
            body.len() <= max,
            "the request of {} bytes passes the bound of {max} bytes",
            body.len()
        );
        let reply = serve(&node, &admin(), &body).await;
        assert_eq!(split(&reply), (large(), &b""[..]));
    });
}

/// The rest after the second problem has one digit fewer than after the first, so
/// `serve` keeps the second only when it counts the rest after each problem.
#[test]
fn a_plan_error_over_the_bound_counts_the_rest_after_each_problem() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh);
        let max = usize::try_from(BODY_BYTES_MAX).expect("a usize");
        let block = "nope \"a\" {}\n";
        let rest = block.repeat(10);
        let kept = |path: &str| {
            let files = [
                File {
                    path: path.into(),
                    text: block.to_owned(),
                },
                File {
                    path: "b.hcl".into(),
                    text: rest.clone(),
                },
            ];
            let documents =
                plan::Documents::read(&files, &node.front_ends).expect("documents");
            let error = documents
                .names(node.supervisor.kinds())
                .expect_err("an error");
            let mut kept = error.problems()[..2].to_vec();
            kept.push(Problem::more(9));
            reply_of(&kept)
        };
        let short = kept("a.hcl").len();
        let path = format!("{}.hcl", "a".repeat(1 + max - short));
        let expected = kept(&path);
        assert_eq!(expected.len(), max);
        let body = plan_body(&[(&path, block), ("b.hcl", &rest)]);
        assert!(
            body.len() <= max,
            "the request of {} bytes passes the bound of {max} bytes",
            body.len()
        );
        assert_eq!(serve(&node, &admin(), &body).await, expected);
    });
}

#[test]
fn gives_the_reply_of_an_error_as_one_line() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh);
        let reply = serve(&node, &admin(), b"{\"version\":{}}\n").await;
        let expected = "{\"errors\":[{\"code\":\"ops.bad-request\",\"message\":\"the \
                        body is no request: the operation `version` does not run on \
                        a node\",\"fix\":\"Use a `foundation` program of the same \
                        version as the node\",\"notes\":[]}],\"status\":1}\n";
        assert_eq!(String::from_utf8(reply).as_deref(), Ok(expected));
    });
}

#[test]
fn refuses_each_body_that_is_no_request() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let after = [&plan_body(&[("site.hcl", &site())])[..], b"x"].concat();
        let cases: [(&[u8], &str); 18] = [
            (b"", "no newline ends the first line"),
            (b"{\"apply\":{}}", "no newline ends the first line"),
            (b"\n", "EOF while parsing a value at line 1 column 0"),
            (b"nope\n", "expected ident at line 1 column 2"),
            (
                b"[]\n",
                "invalid type: sequence, expected an object with one operation at \
                 line 1 column 0",
            ),
            (b"{}\n", "no operation at line 1 column 2"),
            (
                b"{\"apply\":{},\"plan\":{\"files\":[]}}\n",
                "a second operation at line 1 column 18",
            ),
            (
                b"{\"apply\":{},\"apply\":{}}\n",
                "a second operation at line 1 column 19",
            ),
            (
                b"{\"plan\":{}}\n",
                "missing field `files` at line 1 column 10",
            ),
            (
                b"{\"plan\":{\"files\":[{\"path\":\"a.hcl\",\"text\":\"\",\
                  \"mode\":1}]}}\n",
                "unknown field `mode`, expected `path` or `text` at line 1 column 50",
            ),
            (
                b"{\"plan\":{\"files\":[],\"force\":true}}\n",
                "unknown field `force`, expected `files` at line 1 column 27",
            ),
            (
                b"{\"apply\":{\"file\":\"site.plan\"}}\n",
                "unknown field `file`, there are no fields at line 1 column 16",
            ),
            (
                b"{\"apply\":[]}\n",
                "invalid type: sequence, expected an object at line 1 column 9",
            ),
            (
                b"{\"plan\":[[]]}\n",
                "invalid type: sequence, expected an object at line 1 column 8",
            ),
            (
                b"{\"plan\":{\"files\":[[\"a.hcl\",\"\"]]}}\n",
                "invalid type: sequence, expected an object at line 1 column 18",
            ),
            (
                b"{\"status\":[]}\n",
                "invalid type: sequence, expected an object at line 1 column 10",
            ),
            (&after, "bytes follow the first line of `plan`"),
            (
                b"{\"docs\":{}}\n",
                "the operation `docs` does not run on a node",
            ),
        ];
        for (body, message) in cases {
            let reply = serve(&node, &admin(), body).await;
            let text = String::from_utf8_lossy(body);
            assert_eq!(split(&reply), (bad(message), &b""[..]), "{text}");
        }
        assert_eq!(mesh.pointer().version, 0);
    });
}

#[test]
fn quotes_the_first_bytes_of_a_long_cause_for_each_path() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh);
        let mib = 1 << 20;
        let name = "é".repeat(mib / 2);
        let string = "a".repeat(mib);
        let fits = "b".repeat(256 - "the operation `` does not run on a node".len());
        let over = format!("{fits}c");
        let cases = [
            (
                format!("{{\"{fits}\":{{}}}}\n"),
                format!("the operation `{fits}` does not run on a node"),
            ),
            (
                format!("{{\"{over}\":{{}}}}\n"),
                format!("the operation `{over}` does not run on a nod..."),
            ),
            (
                format!("{{\"{name}\":{{}}}}\n"),
                format!("the operation `{}...", &name[..240]),
            ),
            (
                format!("{{\"plan\":\"{string}\"}}\n"),
                format!("invalid type: string \"{}...", &string[..234]),
            ),
        ];
        for (body, message) in cases {
            let reply = serve(&node, &admin(), body.as_bytes()).await;
            assert_eq!(split(&reply), (bad(&message), &b""[..]));
        }
    });
}

#[test]
fn refuses_an_operation_that_the_table_does_not_hold() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh);
        let reply = serve(&node, &admin(), b"{\"export\":{\"to\":1}}\n").await;
        let expected = bad("the operation `export` does not run on a node");
        assert_eq!(split(&reply), (expected, &b""[..]));
    });
}

#[test]
fn gives_each_error_of_plan_and_apply_with_its_status() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let reply = serve(&node, &admin(), &apply_body(b"plan")).await;
        let expected = refusal(
            "ops.bad-plan",
            2,
            &config::plan::Error::Version { found: b'p' }.to_string(),
            "Make a plan with `foundation plan`, and apply it with no edits",
        );
        assert_eq!(split(&reply), (expected, &b""[..]));

        let body = plan_body(&[("site.txt", SITE)]);
        let reply = serve(&node, &admin(), &body).await;
        let expected = json!({
            "errors": [{
                "code": "ops.unknown-extension",
                "message": "no config syntax reads this file",
                "fix": "Use a file that ends in `.hcl`",
                "place": { "file": "site.txt", "line": 1, "column": 1 },
                "notes": [],
            }],
            "status": 2,
        });
        assert_eq!(split(&reply), (expected, &b""[..]));

        let base = mesh.pointer();
        let plan = planned(&node, &[("site.hcl", &placed_site())]).await;
        let reply = serve(&node, &admin(), &apply_body(&plan)).await;
        assert!(split(&reply).0.get("output").is_some());
        let reply = serve(&node, &admin(), &apply_body(&plan)).await;
        let expected = stale(base, mesh.pointer());
        assert_eq!(split(&reply), (expected, &b""[..]));

        let plan = planned(&node, &[("plant.hcl", plant())]).await;
        let stopped = fail_sync(&fixture.sim);
        let expected = refusal(
            "ops.stopped",
            1,
            &stopped.to_string(),
            "Fix the cause in the message, then start the node and plan again",
        );
        let reply = serve(&node, &admin(), &apply_body(&plan)).await;
        assert_eq!(split(&reply), (expected.clone(), &b""[..]));
        let body = plan_body(&[("plant.hcl", plant())]);
        let reply = serve(&node, &admin(), &body).await;
        assert_eq!(split(&reply), (expected, &b""[..]));
    });
}

#[test]
fn refuses_a_plan_with_a_secret_of_the_node_that_has_no_value() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh);
        let c = "connector \"c\" {\n  kind  = \"influx\"\n  node  = \"edge\"\n  \
                 token = env(\"TOKEN\")\n}\n";
        let reply = serve(&node, &admin(), &plan_body(&[("c.hcl", c)])).await;
        let expected = json!({
            "errors": [{
                "code": "secret.unresolved",
                "message": "the secret env(\"TOKEN\"): the environment variable TOKEN of \
                            the node is not set. Start the node with it set to the \
                            value",
                "fix": "Give the node the value at the source, or change the source, \
                        then plan again",
                "place": { "file": "c.hcl", "line": 4, "column": 11 },
                "notes": [],
            }],
            "status": 2,
        });
        assert_eq!(split(&reply), (expected, &b""[..]));
    });
}

#[test]
fn breaker_a_subject_that_may_only_plan_learns_nothing_of_the_files_of_the_node() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        applied(&node, &[("people.hcl", &people)]).await;
        let paths = fixture.sim.paths();
        fixture.sim.make_dir(Path::new("/run"));
        paths
            .write(Path::new("/run/there"), b"x")
            .expect("writes it");
        let probe = |path: &str| {
            format!(
                "connector \"c\" {{\n  kind  = \"influx\"\n  node  = \"edge\"\n  \
                 token = file(\"{path}\")\n}}\n"
            )
        };
        let there = probe("/run/there");
        let absent = probe("/run/absent");
        let body = |c: &str| plan_body(&[("people.hcl", &people), ("c.hcl", c)]);
        // `bob` may plan each name, and apply none.
        let seen = serve(&node, &name("bob"), &body(&there)).await;
        let unseen = serve(&node, &name("bob"), &body(&absent)).await;
        assert_eq!(split(&seen).0, split(&unseen).0);
    });
}

#[test]
fn breaker_a_subject_that_may_not_apply_a_connector_learns_nothing_of_its_sources() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        // `bob` may plan each name and apply each name under `site`, but has no
        // `admin`, which `apply` of a connector needs.
        let people = format!(
            "{}access \"site-apply\" {{\n  subjects = \"bob\"\n  select = \
             \"site.**\"\n  allow = [apply]\n}}\n",
            people()
        );
        applied(&node, &[("people.hcl", &people)]).await;
        let paths = fixture.sim.paths();
        fixture.sim.make_dir(Path::new("/run"));
        paths
            .write(Path::new("/run/there"), b"x")
            .expect("writes it");
        let probe = |path: &str| {
            format!(
                "connector \"site.c\" {{\n  kind  = \"influx\"\n  node  = \"edge\"\n  \
                 token = file(\"{path}\")\n}}\n"
            )
        };
        let there = probe("/run/there");
        let absent = probe("/run/absent");
        let body = |c: &str| plan_body(&[("people.hcl", &people), ("c.hcl", c)]);
        // `bob` may not apply the connector.
        let plan = planned(&node, &[("people.hcl", &people), ("c.hcl", &there)]).await;
        let reply = serve(&node, &name("bob"), &apply_body(&plan)).await;
        let expected = denied("bob", "admin", "`site.c`");
        assert_eq!(split(&reply), (expected, &b""[..]));
        let seen = serve(&node, &name("bob"), &body(&there)).await;
        let unseen = serve(&node, &name("bob"), &body(&absent)).await;
        assert_eq!(split(&seen).0, split(&unseen).0);
    });
}

#[test]
fn breaker_a_subject_with_admin_and_no_apply_learns_nothing_of_its_sources() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        // `bob` may plan each name and has `admin` under `site`, but no `apply`.
        let people = format!(
            "{}access \"site-admin\" {{\n  subjects = \"bob\"\n  select = \
             \"site.**\"\n  allow = [admin]\n}}\n",
            people()
        );
        applied(&node, &[("people.hcl", &people)]).await;
        let paths = fixture.sim.paths();
        fixture.sim.make_dir(Path::new("/run"));
        paths
            .write(Path::new("/run/there"), b"x")
            .expect("writes it");
        let probe = |path: &str| {
            format!(
                "connector \"site.c\" {{\n  kind  = \"influx\"\n  node  = \"edge\"\n  \
                 token = file(\"{path}\")\n}}\n"
            )
        };
        let there = probe("/run/there");
        let absent = probe("/run/absent");
        let body = |c: &str| plan_body(&[("people.hcl", &people), ("c.hcl", c)]);
        let plan = planned(&node, &[("people.hcl", &people), ("c.hcl", &there)]).await;
        let reply = serve(&node, &name("bob"), &apply_body(&plan)).await;
        let expected = denied("bob", "apply", "`site.c`");
        assert_eq!(split(&reply), (expected, &b""[..]));
        let seen = serve(&node, &name("bob"), &body(&there)).await;
        let unseen = serve(&node, &name("bob"), &body(&absent)).await;
        assert_eq!(split(&seen).0, split(&unseen).0);
    });
}

#[test]
fn refuses_a_plan_of_a_subject_with_no_grant_before_it_reads_a_secret() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        applied(&node, &[("people.hcl", &people)]).await;
        let c = "connector \"c\" {\n  kind  = \"influx\"\n  node  = \"edge\"\n  \
                 token = env(\"TOKEN\")\n}\n";
        let body = plan_body(&[("people.hcl", &people), ("c.hcl", c)]);
        let reply = serve(&node, &name("eve"), &body).await;
        let expected = denied("eve", "plan", "`bob.@subject`");
        assert_eq!(split(&reply), (expected, &b""[..]));
    });
}

#[test]
fn refuses_a_plan_of_a_subject_with_no_grant_on_a_name_of_the_files() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        applied(&node, &[("people.hcl", &people())]).await;
        let site = plan_body(&[("site.hcl", &placed_site())]);

        let reply = serve(&node, &name("eve"), &site).await;
        let expected = denied("eve", "plan", "`p.@placement`");
        assert_eq!(split(&reply), (expected, &b""[..]));
        // A subject that the spec does not hold has no grant.
        let reply = serve(&node, &name("mallory"), &site).await;
        let expected = denied("mallory", "plan", "`p.@placement`");
        assert_eq!(split(&reply), (expected, &b""[..]));
        // `sam` may plan the names under `site`, and not the placement.
        let reply = serve(&node, &name("sam"), &site).await;
        let expected = denied("sam", "plan", "`p.@placement`");
        assert_eq!(split(&reply), (expected, &b""[..]));
    });
}

#[test]
fn refuses_a_plan_of_a_subject_with_no_grant_on_a_name_of_the_spec() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        applied(&node, &[("people.hcl", &people)]).await;
        // The files of `sam` hold only names under `site`. The spec in use also
        // holds the subjects and the access policies, which `plan` would remove.
        let site = site();
        let reply =
            serve(&node, &name("sam"), &plan_body(&[("site.hcl", &site)])).await;
        let expected = denied(
            "sam",
            "plan",
            "a definition of the spec in use that the files do not hold",
        );
        assert_eq!(split(&reply), (expected, &b""[..]));

        let both = [("people.hcl", &*people), ("site.hcl", &*site)];
        let reply = serve(&node, &name("bob"), &plan_body(&both)).await;
        let (head, plan) = split(&reply);
        assert_eq!(head["output"]["added"], 3, "{head}");
        assert_eq!(plan, planned(&node, &both).await);
    });
}

#[test]
fn plans_for_a_subject_with_no_grant_on_a_founding_definition() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let policy = "access \"site.surveyor\" {\n  subjects = \"sam\"\n  select = \
                      \"site.**\"\n  allow = [plan]\n}\n";
        applied(&node, &[("people.hcl", policy)]).await;
        let site = site();
        let files = [("people.hcl", policy), ("site.hcl", &*site)];
        let reply = serve(&node, &name("sam"), &plan_body(&files)).await;
        let (head, _) = split(&reply);
        assert_eq!(head["output"]["added"], 3, "{head}");
    });
}

#[test]
fn refuses_an_apply_of_a_subject_with_no_grant_on_a_changed_name() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        applied(&node, &[("people.hcl", &people)]).await;
        let version = mesh.pointer().version;
        let placed = placed_site();
        let files = [("people.hcl", &*people), ("site.hcl", &*placed)];
        let reply = serve(&node, &name("bob"), &plan_body(&files)).await;
        let (head, plan) = split(&reply);
        assert_eq!(head["output"]["added"], 3, "{head}");

        let apply = apply_body(plan);
        let reply = serve(&node, &name("bob"), &apply).await;
        let expected = denied("bob", "apply", "`p.@placement`");
        assert_eq!(split(&reply), (expected, &b""[..]));
        let reply = serve(&node, &name("eve"), &apply).await;
        let expected = denied("eve", "apply", "`p.@placement`");
        assert_eq!(split(&reply), (expected, &b""[..]));
        // `sam` may apply the names under `site`, and not the placement.
        let reply = serve(&node, &name("sam"), &apply).await;
        let expected = denied("sam", "apply", "`p.@placement`");
        assert_eq!(split(&reply), (expected, &b""[..]));
        assert_eq!(mesh.pointer().version, version);
    });
}

#[test]
fn applies_for_a_subject_with_a_grant_on_each_changed_name_and_each_home() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        let placement = site().replace(SITE, "");
        applied(&node, &[("people.hcl", &people), ("p.hcl", &placement)]).await;
        let version = mesh.pointer().version;
        let site = site();
        let files = [("people.hcl", &*people), ("site.hcl", &*site)];
        let plan = planned(&node, &files).await;
        // `sam` has no `plan` on each name of the spec in use, and applies a plan
        // that changes only names under `site`.
        let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
        let (head, _) = split(&reply);
        assert_eq!(head["output"]["added"], 2, "{head}");
        assert_eq!(head["output"]["homes"], 1, "{head}");
        assert_eq!(mesh.pointer().version, version + 1);

        // A change of a channel needs `apply` alone.
        let errors = site.replace(
            "{ kind = \"index\" }",
            "{\n  kind = \"index\"\n  error = \"site.temp\"\n}",
        );
        let files = [("people.hcl", &*people), ("site.hcl", &*errors)];
        let plan = planned(&node, &files).await;
        let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
        let (head, _) = split(&reply);
        assert_eq!(head["output"]["changed"], 1, "{head}");
        assert_eq!(mesh.pointer().version, version + 2);

        // A removal of a channel needs `apply` alone.
        let time = "channel \"site.time\" { kind = \"index\" }\n";
        let files = [
            ("people.hcl", &*people),
            ("p.hcl", &placement),
            ("t.hcl", time),
        ];
        let plan = planned(&node, &files).await;
        let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
        let (head, _) = split(&reply);
        assert_eq!(head["output"]["removed"], 1, "{head}");
        assert_eq!(mesh.pointer().version, version + 3);
    });
}

/// The home of the index `index` in `mesh`.
async fn home(mesh: &mesh::Mesh, index: &str) -> Option<types::node::Key> {
    let spec = mesh.spec().await.expect("a spec");
    let Definition::Channel(channel) = &spec.definitions[&name(index)] else {
        panic!("a channel at {index}");
    };
    mesh.watch(channel.key)
        .next()
        .await
        .expect("a running mesh")
}

/// The plan of `files` that `@admin` makes and applies with no home, and the homes
/// that it leaves out.
async fn applied_with_no_home(
    node: &Node,
    files: &[(&str, &str)],
) -> BTreeMap<Name, Name> {
    let plan = planned(node, files).await;
    let mut plan = config::plan::Plan::decode(&plan).expect("a plan");
    let homes = std::mem::take(&mut plan.homes);
    let reply = serve(node, &admin(), &apply_body(&plan.encode())).await;
    assert!(split(&reply).0.get("output").is_some());
    homes
}

#[test]
fn an_apply_gives_no_home_to_an_index_on_which_the_subject_has_no_grant() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        applied(&node, &[("people.hcl", &people)]).await;
        let placed = placed_site();
        let files = [("people.hcl", &*people), ("site.hcl", &*placed)];
        let plan = planned(&node, &files).await;
        let site = config::plan::Plan::decode(&plan).expect("a plan");
        let homes = applied_with_no_home(&node, &files).await;
        assert_eq!(homes.keys().collect::<Vec<_>>(), [&name("site.time")]);
        let mut only = site;
        only.base = mesh.pointer();
        only.changes.clear();
        only.homes = homes;
        let version = mesh.pointer().version;

        let apply = apply_body(&only.encode());
        // With the home dropped, the plan of `bob` names no definition.
        let reply = serve(&node, &name("bob"), &apply).await;
        let expected = denied("bob", "apply", NOTHING);
        assert_eq!(split(&reply), (expected, &b""[..]));
        assert_eq!(mesh.pointer().version, version);
        assert_eq!(home(&mesh, "site.time").await, None);
        let reply = serve(&node, &name("sam"), &apply).await;
        let (head, _) = split(&reply);
        assert_eq!(head["output"]["homes"], 1, "{head}");
        assert_eq!(mesh.pointer().version, version + 1);
        assert_eq!(home(&mesh, "site.time").await, Some(NODE));
    });
}

#[test]
fn an_apply_gives_a_home_only_to_the_index_of_the_grant_of_the_subject() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        applied(&node, &[("people.hcl", &people)]).await;
        let site = site();
        let yard = "channel \"yard.time\" { kind = \"index\" }\nplacement \
                    \"yard.home\" {\n  select = \"yard.*\"\n  home = \"edge\"\n}\n";
        let files = [
            ("people.hcl", &*people),
            ("site.hcl", &*site),
            ("yard.hcl", yard),
        ];
        let homes = applied_with_no_home(&node, &files).await;
        assert_eq!(homes.len(), 2);
        let version = mesh.pointer().version;
        // `sam` may apply `site.hum`, and has no grant on `yard.time`.
        let hum = format!(
            "{site}channel \"site.hum\" {{\n  data_type = \"f64\"\n  index = \
             \"site.time\"\n}}\n"
        );
        let files = [
            ("people.hcl", &*people),
            ("site.hcl", &*hum),
            ("yard.hcl", yard),
        ];
        let plan = planned(&node, &files).await;

        let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
        let (head, _) = split(&reply);
        assert_eq!(head["output"]["added"], 1, "{head}");
        assert_eq!(head["output"]["homes"], 1, "{head}");
        assert_eq!(mesh.pointer().version, version + 1);
        assert_eq!(home(&mesh, "site.time").await, Some(NODE));
        assert_eq!(home(&mesh, "yard.time").await, None);
    });
}

#[test]
fn refuses_a_subject_with_no_grant_before_it_compares_the_base_of_a_plan() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let base = mesh.pointer();
        let plan = planned(&node, &[("site.hcl", &site())]).await;
        applied(&node, &[("people.hcl", &people())]).await;
        let apply = apply_body(&plan);
        let reply = serve(&node, &name("eve"), &apply).await;
        let expected = denied("eve", "apply", "`site.home.@placement`");
        assert_eq!(split(&reply), (expected, &b""[..]));
        let reply = serve(&node, &admin(), &apply).await;
        let expected = stale(base, mesh.pointer());
        assert_eq!(split(&reply), (expected, &b""[..]));
    });
}

#[test]
fn refuses_a_plan_and_an_apply_on_a_node_that_uses_an_old_spec_as_behind() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let site = placed_site();
        applied(&node, &[("site.hcl", &site)]).await;
        let first = mesh.pointer();
        let both = [("site.hcl", &*site), ("plant.hcl", plant())];
        let plan = planned(&node, &both).await;
        fixture
            .sim
            .fail_file(Path::new("spec"), env::files::Operation::SyncDir);
        let reply = serve(&node, &admin(), &apply_body(&plan)).await;
        assert!(split(&reply).0.get("output").is_some());
        let spec = mesh.spec().await.expect("a spec");
        assert_eq!(spec.pointer, Some(first));
        let behind = spec.behind.expect("a node behind");
        let Cause::Files(error) = &behind.cause else {
            panic!("a pointer file that is not durable, not {behind:?}");
        };
        let message = format!(
            "the node does not use the newest spec, at {}: the file of the pointer \
             in use was not made durable: {error}",
            behind.pointer
        );
        let expected =
            refusal("ops.behind", 1, &message, "Fix the cause, then plan again");

        let reply = serve(&node, &admin(), &plan_body(&both)).await;
        assert_eq!(split(&reply), (expected.clone(), &b""[..]));
        let reply = serve(&node, &admin(), &apply_body(&plan)).await;
        assert_eq!(split(&reply), (expected, &b""[..]));
        // The check of the subject comes first, on the rules of the spec in use.
        let reply = serve(&node, &name("eve"), &plan_body(&both)).await;
        let expected = denied("eve", "plan", "`influx`");
        assert_eq!(split(&reply), (expected, &b""[..]));
    });
}

#[test]
fn applies_a_plan_that_removes_the_policy_of_the_subject() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let policy = "access \"site.surveyor\" {\n  subjects = \"sam\"\n  select = \
                      \"site.**\"\n  allow = [plan, apply, admin]\n}\n";
        applied(&node, &[("people.hcl", policy)]).await;
        let version = mesh.pointer().version;
        let reply = serve(&node, &name("sam"), &plan_body(&[])).await;
        let (head, plan) = split(&reply);
        assert_eq!(head["output"]["removed"], 1, "{head}");

        let reply = serve(&node, &name("sam"), &apply_body(plan)).await;
        let (head, _) = split(&reply);
        assert_eq!(head["output"]["removed"], 1, "{head}");
        assert_eq!(mesh.pointer().version, version + 1);
    });
}

#[test]
fn refuses_a_change_of_access_of_a_subject_with_no_admin_on_its_key() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        applied(&node, &[("people.hcl", &people)]).await;
        let version = mesh.pointer().version;
        // A policy governs its whole region, whatever its key.
        let policy = "access \"site.evil\" {\n  subjects = \"sam\"\n  select = \
                      \"**\"\n  allow = [plan, apply]\n}\n";
        let mole = format!("subject \"site.mole\" {{ keys = [\"{EVE}\"] }}\n");
        let cases = [
            (policy, "`site.evil.@access`"),
            (&*mole, "`site.mole.@subject`"),
        ];
        for (text, key) in cases {
            let files = [("people.hcl", &*people), ("evil.hcl", text)];
            let plan = planned(&node, &files).await;
            let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
            let expected = denied("sam", "admin", key);
            assert_eq!(split(&reply), (expected, &b""[..]));
        }
        assert_eq!(mesh.pointer().version, version);

        let admin =
            people.replace("allow = [plan, apply]", "allow = [plan, apply, admin]");
        applied(&node, &[("people.hcl", &admin)]).await;
        for (text, _) in cases {
            let version = mesh.pointer().version;
            let files = [("people.hcl", &*admin), ("evil.hcl", text)];
            let plan = planned(&node, &files).await;
            let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
            let (head, _) = split(&reply);
            assert_eq!(head["output"]["added"], 1, "{head}");
            assert_eq!(mesh.pointer().version, version + 1);
        }
    });
}

#[test]
fn refuses_a_policy_of_a_subject_with_no_admin_on_its_key() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        let yard = "channel \"yard.time\" { kind = \"index\" }\nplacement \
                    \"yard.home\" {\n  select = \"yard.*\"\n  home = \"edge\"\n}\n";
        applied(&node, &[("people.hcl", &people), ("yard.hcl", yard)]).await;
        let version = mesh.pointer().version;
        // `sam` has no grant on `yard.time`, which each policy selects.
        let wipe =
            "retention \"site.wipe\" {\n  select = \"yard.*\"\n  keep = \"0s\"\n}\n";
        let away = "placement \"site.away\" {\n  select = \"yard.time\"\n  home = \
                    \"edge\"\n}\n";
        let big =
            "node_settings \"site.big\" {\n  select = \"**\"\n  disk = \"1GiB\"\n}\n";
        let cases = [
            (wipe, "`site.wipe.@retention`"),
            (away, "`site.away.@placement`"),
            (big, "`site.big.@node_settings`"),
        ];
        for (text, key) in cases {
            let files = [
                ("people.hcl", &*people),
                ("yard.hcl", yard),
                ("p.hcl", text),
            ];
            let plan = planned(&node, &files).await;
            let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
            let expected = denied("sam", "admin", key);
            assert_eq!(split(&reply), (expected, &b""[..]), "{text}");
        }
        assert_eq!(mesh.pointer().version, version);

        let admin =
            people.replace("allow = [plan, apply]", "allow = [plan, apply, admin]");
        applied(&node, &[("people.hcl", &admin), ("yard.hcl", yard)]).await;
        for (text, _) in cases {
            let version = mesh.pointer().version;
            let files = [("people.hcl", &*admin), ("yard.hcl", yard), ("p.hcl", text)];
            let plan = planned(&node, &files).await;
            let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
            let (head, _) = split(&reply);
            assert_eq!(head["output"]["added"], 1, "{head}");
            assert_eq!(mesh.pointer().version, version + 1);
        }
    });
}

/// A yard on which `sam` has no grant: an index, two data channels on it, and a
/// placement that homes each index on `edge`.
const YARD: &str = "channel \"yard.time\" { kind = \"index\" }\nchannel \"yard.temp\" \
                    {\n  data_type = \"f64\"\n  index = \"yard.time\"\n}\nchannel \
                    \"yard.ctl\" {\n  data_type = \"string\"\n  index = \
                    \"yard.time\"\n}\nplacement \"yard.home\" {\n  select = \"**\"\n  \
                    home = \"edge\"\n}\n";

#[test]
fn refuses_a_connector_of_a_subject_with_no_admin_on_its_key() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        applied(&node, &[("people.hcl", &people), ("yard.hcl", YARD)]).await;
        let version = mesh.pointer().version;
        // The connector writes `yard.temp`, on which `sam` has no grant.
        let plc = "connector \"site.plc\" {\n  kind = \"opcua\"\n  node = \"edge\"\n  \
                   read \"yard.temp\" { node_id = \"x\" }\n}\n";
        let files = [("people.hcl", &*people), ("yard.hcl", YARD), ("p.hcl", plc)];
        let plan = planned(&node, &files).await;
        let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
        let expected = denied("sam", "admin", "`site.plc`");
        assert_eq!(split(&reply), (expected, &b""[..]));
        assert_eq!(mesh.pointer().version, version);

        let admin =
            people.replace("allow = [plan, apply]", "allow = [plan, apply, admin]");
        applied(&node, &[("people.hcl", &admin), ("yard.hcl", YARD)]).await;
        let version = mesh.pointer().version;
        let files = [("people.hcl", &*admin), ("yard.hcl", YARD), ("p.hcl", plc)];
        let plan = planned(&node, &files).await;
        let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
        let (head, _) = split(&reply);
        // The connector and its thirteen status channels.
        assert_eq!(head["output"]["added"], 14, "{head}");
        assert_eq!(mesh.pointer().version, version + 1);
    });
}

#[test]
fn refuses_a_channel_over_a_connector_of_a_subject_with_no_admin_on_its_key() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        let plc = "connector \"site.plc\" {\n  kind = \"opcua\"\n  node = \"edge\"\n  \
                   read \"yard.temp\" { node_id = \"x\" }\n}\n";
        let files = [("people.hcl", &*people), ("yard.hcl", YARD), ("p.hcl", plc)];
        applied(&node, &files).await;
        let version = mesh.pointer().version;
        let channel = "channel \"site.plc\" { kind = \"index\" }\n";
        let files = [
            ("people.hcl", &*people),
            ("yard.hcl", YARD),
            ("p.hcl", channel),
        ];
        let plan = planned(&node, &files).await;
        let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
        let expected = denied("sam", "admin", "`site.plc`");
        assert_eq!(split(&reply), (expected, &b""[..]));
        assert_eq!(mesh.pointer().version, version);
    });
}

#[test]
fn refuses_a_connector_over_a_channel_of_a_subject_with_no_admin_on_its_key() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        let channel = "channel \"site.plc\" { kind = \"index\" }\n";
        let files = [
            ("people.hcl", &*people),
            ("yard.hcl", YARD),
            ("p.hcl", channel),
        ];
        applied(&node, &files).await;
        let version = mesh.pointer().version;
        let plc = "connector \"site.plc\" {\n  kind = \"opcua\"\n  node = \"edge\"\n  \
                   read \"yard.temp\" { node_id = \"x\" }\n}\n";
        let files = [("people.hcl", &*people), ("yard.hcl", YARD), ("p.hcl", plc)];
        let plan = planned(&node, &files).await;
        let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
        let expected = denied("sam", "admin", "`site.plc`");
        assert_eq!(split(&reply), (expected, &b""[..]));
        assert_eq!(mesh.pointer().version, version);
    });
}

/// A connector under `site` that reads `yard.temp`, with thirteen status channels.
const PLC: &str = "connector \"site.plc\" {\n  kind = \"opcua\"\n  node = \"edge\"\n  \
                   read \"yard.temp\" { node_id = \"x\" }\n}\n";

/// A plan file that holds the change `edit` of each stored status channel of
/// `site.plc` for which `edit` gives true, made from a plan of `@admin` with
/// `people`. Each change, before `edit`, makes its channel an index.
async fn status_plan(
    node: &Node,
    mesh: &mesh::Mesh,
    people: &str,
    edit: impl Fn(&mut config::plan::Change, &Name) -> bool,
) -> Vec<u8> {
    let index = format!("{PLC}channel \"site.t\" {{ kind = \"index\" }}\n");
    let files = [
        ("people.hcl", people),
        ("yard.hcl", YARD),
        ("p.hcl", &*index),
    ];
    let plan = planned(node, &files).await;
    let mut plan = config::plan::Plan::decode(&plan).expect("a plan");
    let template = plan.changes.remove(&name("site.t")).expect("a change");
    let spec = mesh.spec().await.expect("a spec");
    for (channel, definition) in spec.definitions.iter() {
        if channel.as_str().starts_with("site.plc.status.") {
            let mut change = template.clone();
            change.old = Some(types::digest::Digest::of(&definition.encode()));
            if edit(&mut change, channel) {
                plan.changes.insert(channel.clone(), change);
            }
        }
    }
    plan.homes.clear();
    plan.encode()
}

/// The first line of the reply of `config.implied-channel` for each status channel
/// of `site.plc` that `last` names by its last segment.
fn unheld(last: &[&str]) -> Value {
    let errors: Vec<Value> = last
        .iter()
        .map(|last| {
            let message = format!(
                "the connector `site.plc` implies the channel \
                 `site.plc.status.{last}`, so the spec must hold it as the connector \
                 gives it"
            );
            json!({
                "code": "config.implied-channel",
                "message": message,
                "fix": "Make the plan file with `plan`",
                "notes": [],
            })
        })
        .collect();
    json!({ "errors": errors, "status": 2 })
}

#[test]
fn refuses_an_apply_that_removes_the_status_channels_of_a_connector() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        let files = [("people.hcl", &*people), ("yard.hcl", YARD), ("p.hcl", PLC)];
        applied(&node, &files).await;
        let version = mesh.pointer().version;
        let plan = status_plan(&node, &mesh, &people, |change, _| {
            change.new = None;
            true
        })
        .await;
        let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
        let all = [
            "time",
            "state",
            "class",
            "restarts",
            "backoff",
            "error",
            "note",
            "in",
            "lost",
            "missed",
            "bad",
            "uncertain",
            "mistyped",
        ];
        assert_eq!(split(&reply), (unheld(&all), &b""[..]));
        assert_eq!(mesh.pointer().version, version);
    });
}

#[test]
fn refuses_an_apply_that_makes_the_status_data_of_a_connector_indexes() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        let files = [("people.hcl", &*people), ("yard.hcl", YARD), ("p.hcl", PLC)];
        applied(&node, &files).await;
        let version = mesh.pointer().version;
        let plan = status_plan(&node, &mesh, &people, |_, channel| {
            *channel != name("site.plc.status.time")
        })
        .await;
        let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
        let data = [
            "state",
            "class",
            "restarts",
            "backoff",
            "error",
            "note",
            "in",
            "lost",
            "missed",
            "bad",
            "uncertain",
            "mistyped",
        ];
        assert_eq!(split(&reply), (unheld(&data), &b""[..]));
        assert_eq!(mesh.pointer().version, version);
    });
}

/// Only the connector writes its status, so a subject with no `admin` cannot put a
/// new channel on the status index of a connector.
#[test]
fn refuses_an_apply_that_puts_a_channel_on_the_status_index_of_a_connector() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        let files = [("people.hcl", &*people), ("yard.hcl", YARD), ("p.hcl", PLC)];
        applied(&node, &files).await;
        let version = mesh.pointer().version;
        let x = format!(
            "{PLC}channel \"site.t\" {{ kind = \"index\" }}\nchannel \"site.x\" \
             {{\n  data_type = \"f64\"\n  index = \"site.t\"\n}}\n"
        );
        let files = [("people.hcl", &*people), ("yard.hcl", YARD), ("p.hcl", &*x)];
        let plan = planned(&node, &files).await;
        let mut plan = config::plan::Plan::decode(&plan).expect("a plan");
        plan.changes.remove(&name("site.t")).expect("a change");
        plan.homes.clear();
        let change = plan.changes.get_mut(&name("site.x")).expect("a change");
        let new = change.new.as_mut().expect("a new channel");
        let data_type = "f64".parse().expect("a type");
        let index = name("site.plc.status.time");
        let data = spec::channel::Data::new(index, None, data_type, None);
        let data = spec::channel::Kind::Data(data.expect("no unit"));
        new.definition = config::Definition::Channel(data);
        let reply = serve(&node, &name("sam"), &apply_body(&plan.encode())).await;
        let message = "the connector `site.plc` implies the channel \
                       `site.plc.status.time`, so the channel `site.x` cannot point \
                       at it";
        let error = json!({
            "code": "config.implied-channel",
            "message": message,
            "fix": "Make the plan file with `plan`",
            "notes": [],
        });
        let head = json!({ "errors": [error], "status": 2 });
        assert_eq!(split(&reply), (head, &b""[..]));
        assert_eq!(mesh.pointer().version, version);
    });
}

/// A hand-made plan can remove a name that the spec does not hold, and the check
/// refuses it before `Plan::definitions` reads the spec.
#[test]
fn refuses_a_removal_of_a_name_that_the_spec_does_not_hold_with_no_admin() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        let site = site();
        applied(&node, &[("people.hcl", &*people), ("site.hcl", &*site)]).await;
        let version = mesh.pointer().version;
        let placement = site.replace(SITE, "");
        let files = [("people.hcl", &*people), ("p.hcl", &*placement)];
        let plan = planned(&node, &files).await;
        let mut plan = config::plan::Plan::decode(&plan).expect("a plan");
        let change = plan.changes.remove(&name("site.time")).expect("a removal");
        plan.changes.insert(name("site.ghost"), change);
        let reply = serve(&node, &name("sam"), &apply_body(&plan.encode())).await;
        let expected = denied("sam", "admin", "`site.ghost`");
        assert_eq!(split(&reply), (expected, &b""[..]));
        assert_eq!(mesh.pointer().version, version);
    });
}

#[test]
fn refuses_a_channel_with_an_edge_to_a_channel_with_no_grant() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        applied(&node, &[("people.hcl", &people), ("yard.hcl", YARD)]).await;
        let version = mesh.pointer().version;
        let x = "channel \"site.x\" {\n  data_type = \"f64\"\n  index = \
                 \"yard.time\"\n}\n";
        let time = "channel \"site.time\" {\n  kind = \"index\"\n  control = \
                    \"yard.ctl\"\n}\n";
        // `sam` has no `admin` on the policy, and the edge check comes first.
        let wipe = format!(
            "{time}retention \"site.wipe\" {{\n  select = \"site.*\"\n  keep = \
             \"0s\"\n}}\n"
        );
        let cases = [
            (x, "`yard.time`"),
            (time, "`yard.ctl`"),
            (&*wipe, "`yard.ctl`"),
        ];
        for (text, on) in cases {
            let files = [
                ("people.hcl", &*people),
                ("yard.hcl", YARD),
                ("s.hcl", text),
            ];
            let plan = planned(&node, &files).await;
            let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
            let expected = denied("sam", "apply", on);
            assert_eq!(split(&reply), (expected, &b""[..]), "{text}");
        }
        assert_eq!(mesh.pointer().version, version);

        // A change of a channel that `sam` may apply.
        let own = SITE.replace("site.temp", "site.x");
        let files = [
            ("people.hcl", &*people),
            ("yard.hcl", YARD),
            ("s.hcl", &*own),
        ];
        applied(&node, &files).await;
        let version = mesh.pointer().version;
        let moved = own.replace("index = \"site.time\"", "index = \"yard.time\"");
        let files = [
            ("people.hcl", &*people),
            ("yard.hcl", YARD),
            ("s.hcl", &*moved),
        ];
        let plan = planned(&node, &files).await;
        let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
        let expected = denied("sam", "apply", "`yard.time`");
        assert_eq!(split(&reply), (expected, &b""[..]));
        assert_eq!(mesh.pointer().version, version);
    });
}

#[test]
fn refuses_a_change_or_removal_of_access_with_no_admin_on_its_key() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let people = people();
        let crew = "access \"site.crew\" {\n  subjects = \"bob\"\n  select = \
                    \"site.*\"\n  allow = [plan]\n}\n";
        let mole = format!("subject \"site.mole\" {{ keys = [\"{EVE}\"] }}\n");
        let both = format!("{crew}{mole}");
        applied(&node, &[("people.hcl", &people), ("crew.hcl", &both)]).await;
        let version = mesh.pointer().version;
        let taken = crew
            .replace("\"bob\"", "\"sam\"")
            .replace("\"site.*\"", "\"**\"")
            .replace("[plan]", "[plan, apply, admin]");
        let rekeyed = mole.replace(EVE, SAM);
        let cases = [
            (format!("{taken}{mole}"), "`site.crew.@access`"),
            (mole.clone(), "`site.crew.@access`"),
            (format!("{crew}{rekeyed}"), "`site.mole.@subject`"),
            (crew.to_owned(), "`site.mole.@subject`"),
        ];
        for (text, key) in &cases {
            let files = [("people.hcl", &*people), ("crew.hcl", &**text)];
            let plan = planned(&node, &files).await;
            let reply = serve(&node, &name("sam"), &apply_body(&plan)).await;
            let expected = denied("sam", "admin", key);
            assert_eq!(split(&reply), (expected, &b""[..]), "{text}");
        }
        assert_eq!(mesh.pointer().version, version);
    });
}

#[test]
fn refuses_a_request_that_names_no_definition_for_each_subject() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        // `plan` compares no founding definition, so the spec has no name to check.
        for subject in ["@admin", "mallory"] {
            let reply = serve(&node, &name(subject), &plan_body(&[])).await;
            let expected = denied(subject, "plan", NOTHING);
            assert_eq!(split(&reply), (expected, &b""[..]));
        }
        let plan = planned(&node, &[("site.hcl", &site())]).await;
        let mut empty = config::plan::Plan::decode(&plan).expect("a plan");
        empty.changes.clear();
        empty.homes.clear();
        applied(&node, &[("people.hcl", &people())]).await;
        let version = mesh.pointer().version;
        empty.base = mesh.pointer();
        let apply = apply_body(&empty.encode());
        for subject in ["@admin", "mallory"] {
            let reply = serve(&node, &name(subject), &apply).await;
            let expected = denied(subject, "apply", NOTHING);
            assert_eq!(split(&reply), (expected, &b""[..]));
        }
        assert_eq!(mesh.pointer().version, version);
    });
}

#[test]
fn refuses_each_remote_subject_on_a_node_that_uses_no_spec() {
    let mut founding = spec::founding::create(crate::common::ADMIN.public());
    let policy = founding[&name("@admin.@access")].clone();
    // An access policy at a key that is no tree key of its kind: a problem.
    founding.insert(name("x"), policy);
    crate::common::founded(&["edge"], founding, |fixture, mesh| async move {
        let node = create_node(&fixture, mesh.clone());
        let spec = mesh.spec().await.expect("a spec");
        assert_eq!(spec.pointer, None, "{spec:?}");
        assert!(spec.behind.is_some(), "{spec:?}");
        let site = site();
        let reply = serve(&node, &admin(), &plan_body(&[("site.hcl", &site)])).await;
        let expected = denied("@admin", "plan", "`site.home.@placement`");
        assert_eq!(split(&reply), (expected, &b""[..]));
        let reply = serve(&node, &admin(), &plan_body(&[])).await;
        let expected = denied("@admin", "plan", NOTHING);
        assert_eq!(split(&reply), (expected, &b""[..]));
        let local = node.plan(vec![(PathBuf::from("site.hcl"), site)]).await;
        let error = local.expect_err("a node behind");
        assert_eq!(error["errors"][0]["code"], "ops.behind", "{error}");
    });
}

/// The `plan` body of `plant()` at a path of `long` bytes, which each change of the
/// reply holds, and of one subject at a path of `short` bytes.
fn wide_body(long: usize, short: usize) -> Vec<u8> {
    let long = format!("{}.hcl", "x".repeat(long - 4));
    let short = format!("{}.hcl", "y".repeat(short - 4));
    let subject = format!("subject \"bob\" {{ keys = [\"{BOB}\"] }}\n");
    plan_body(&[(&long, plant()), (&short, &subject)])
}

#[test]
fn gives_a_reply_at_the_bound_and_refuses_one_over_it() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh);
        let bound = usize::try_from(BODY_BYTES_MAX).expect("the bound fits");
        let reply = serve(&node, &admin(), &plan_body(&[("plant.hcl", plant())])).await;
        let changes = split(&reply).0["output"]["added"]
            .as_u64()
            .expect("a count");
        // Each change of `plant()` holds the long path.
        let long = bound / usize::try_from(changes).expect("the count fits") - 1_000;
        let reply = serve(&node, &admin(), &wide_body(long, 4)).await;
        assert!(split(&reply).0.get("output").is_some());
        let short = 4 + bound - reply.len();

        let reply = serve(&node, &admin(), &wide_body(long, short)).await;
        assert_eq!(reply.len(), bound);
        assert!(split(&reply).0.get("output").is_some());

        let reply = serve(&node, &admin(), &wide_body(long, short + 1)).await;
        assert_eq!(split(&reply), (large(), &b""[..]));

        let reply = serve(&node, &admin(), &plan_body(&[("site.hcl", &site())])).await;
        assert_eq!(split(&reply).0["output"]["added"], 3);
    });
}

#[test]
fn gives_the_problems_that_fit_of_many_at_a_long_path() {
    solo(|fixture, mesh| async move {
        let node = create_node(&fixture, mesh);
        let path = format!("{}.hcl", "x".repeat((64 << 10) - 4));
        let text = "nope \"a\" {}\n".repeat(2_000);
        let reply = serve(&node, &admin(), &plan_body(&[(&path, &text)])).await;
        more_problems(&reply, "document.unknown-block", 2_000, 2);
    });
}

/// A JSON list of `len` strings of 100 bytes, which counts each string it writes.
struct Strings {
    len: usize,
    written: Cell<usize>,
}

impl serde::Serialize for Strings {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut list = serializer.serialize_seq(Some(self.len))?;
        for _ in 0..self.len {
            self.written.set(self.written.get() + 1);
            list.serialize_element(&"x".repeat(98))?;
        }
        list.end()
    }
}

#[test]
fn stops_writing_a_reply_at_the_bound() {
    let bound = usize::try_from(BODY_BYTES_MAX).expect("the bound fits");
    // The JSON of each string is 101 bytes with its comma.
    let fits = (bound - 2) / 101;
    let strings = Strings {
        len: fits,
        written: Cell::new(0),
    };
    Body::line(&strings).expect("a reply at the bound");
    let strings = Strings {
        len: 10 * fits,
        written: Cell::new(0),
    };
    let Err(Large) = Body::line(&strings) else {
        panic!("a reply over the bound fits");
    };
    assert_eq!(strings.written.get(), fits + 1);
}

/// A value that no serializer can write.
struct Unwritable;

impl serde::Serialize for Unwritable {
    fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
        Err(serde::ser::Error::custom("no JSON"))
    }
}

#[test]
#[should_panic(expected = "invariant: a body is plain JSON data: no JSON")]
fn panics_on_a_reply_that_is_no_json_data() {
    if let Err(Large) = Body::line(&Unwritable) {
        panic!("`Large` in place of the panic");
    }
}
