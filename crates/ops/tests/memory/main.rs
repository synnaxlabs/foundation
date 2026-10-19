//! The tests that bound the heap of `serve` and of `ops::roots::Failures` with one
//! count of the bytes they hold. The count covers each thread, so this binary has no
//! test harness. The sim runs on one thread, so the count is exact.

#![expect(clippy::disallowed_macros, reason = "COUNTING ALLOCATOR")]

use std::cell::Cell;
use std::ffi::OsString;
use std::path::Path;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use connector::supervisor::{self, Supervisor};
use ops::{Config, FrontEnd, FrontEnds, Node, serve};
use serde_json::{Value, json};
use sim::Sim;
use types::channel::Key;
use wire::hub::client::BODY_BYTES_MAX;

#[path = "../common/founding.rs"]
mod founding;

use founding::{ADMIN, create_mesh};

#[global_allocator]
static ALLOCATOR: counting::Bytes = counting::Bytes::new();

/// The bytes of the path of the file of each request. Each problem or change of a reply
/// holds it, and the heap holds it once for each request.
const PATH_BYTES: usize = 64 << 10;
/// The problems or changes of each reply: with the path, 8 times the bound.
const ENTRIES: usize = 2_000;
/// The bytes of the request and of its steps, apart from the reply, that the heap can
/// hold at once. The steps took at most 3.9 MB on the laptop (Apple M3 Max).
const STEPS_BYTES: usize = 8 << 20;
/// The times that a plan gives the directory `cfg` of 1000 empty `.hcl` files: a
/// request of 68 MB, 4 times the bound.
const REPEATS: usize = 2_000;

fn main() {
    a_plan_of_one_directory_many_times_holds_at_most_three_bounds();
    an_mcp_plan_of_one_directory_many_times_holds_at_most_three_bounds();
    a_reply_of_many_problems_holds_no_more_than_the_bound();
    a_reply_of_many_changes_holds_no_more_than_the_bound();
    roots_of_many_failures_hold_at_most_a_mebibyte();
}

fn a_plan_of_one_directory_many_times_holds_at_most_three_bounds() {
    let args: Vec<_> = ["plan"]
        .into_iter()
        .chain(std::iter::repeat_n("cfg", REPEATS))
        .collect();
    let (_, errors) = run_on_cfg(&args, None);
    assert!(errors.contains("ops.large-request"), "{errors}");
}

fn an_mcp_plan_of_one_directory_many_times_holds_at_most_three_bounds() {
    let call = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": "plan", "arguments": { "files": vec!["cfg"; REPEATS] } },
    });
    let (output, _) = run_on_cfg(&["mcp"], Some(format!("{call}\n").into_bytes()));
    assert!(output.contains("ops.large-request"), "{output}");
}

/// Runs `foundation` with `args`, and `line` as its input, where `/work/cfg` holds 1000
/// empty `.hcl` files, and returns its output and its errors. It asserts that the heap
/// grows by at most 3 times the bound and `STEPS_BYTES`: each `File` holds 48 bytes and
/// a path of 12 for its 34 bytes of JSON, and its `Vec` grows by doubling.
///
/// # Panics
///
/// When the run sends a request or waits.
fn run_on_cfg(args: &[&str], line: Option<Vec<u8>>) -> (String, String) {
    let mut sim = Sim::new(sim::Config::default());
    let host = sim.node(sim::node::Config::default());
    host.make_dir(Path::new("/work"));
    host.make_dir(Path::new("/work/cfg"));
    host.set_working_dir(Path::new("/work"));
    let paths = host.paths();
    for n in 0..1000 {
        let path = format!("cfg/{n:04}.hcl");
        paths.write(Path::new(&path), b"").expect("a write");
    }
    let line = Cell::new(line);
    let input = async || Ok(line.take().map_or(ops::Line::End, ops::Line::Bytes));
    let request = async |_: &[u8]| -> Result<Vec<u8>, ops::Failure> {
        panic!("a plan past the bound sent a request")
    };
    let reach = async |_: &Path| Ok(request);
    let front_ends = hcl();
    let kinds = connector::kind::Table::new();
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let before = ALLOCATOR.held();
    ALLOCATOR.reset_peak();
    {
        let args = std::iter::once("foundation").chain(args.iter().copied());
        let args = args.map(OsString::from);
        let cli = ops::cli(
            args,
            input,
            &mut output,
            &mut errors,
            &paths,
            &front_ends,
            &kinds,
            reach,
        );
        let Poll::Ready(_) = pin!(cli).poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("the run waited");
        };
    }
    let grown = ALLOCATOR.peak() - before;
    let bound =
        3 * usize::try_from(BODY_BYTES_MAX).expect("the bound fits") + STEPS_BYTES;
    assert!(
        grown <= bound,
        "{} grew the heap by {grown}, past {bound}",
        args[0]
    );
    let text = |bytes| String::from_utf8(bytes).expect("UTF-8");
    (text(output), text(errors))
}

/// The failures of one byte hold their `String`, and also their spare capacity.
fn roots_of_many_failures_hold_at_most_a_mebibyte() {
    hold_at_most_a_mebibyte("of one byte", || "x".to_owned());
    hold_at_most_a_mebibyte("with spare", || {
        let mut failure = String::with_capacity(64);
        failure.push('x');
        failure
    });
}

fn hold_at_most_a_mebibyte(made: &str, failure: impl Fn() -> String) {
    let before = ALLOCATOR.held();
    let failures = (0..100_000).map(|_| failure()).collect();
    let roots = ops::roots::Failures::new(String::new(), failures);
    let held = ALLOCATOR.held() - before;
    assert!(
        held <= 1 << 20,
        "the roots of 100000 failures {made} hold {held} bytes"
    );
    drop(roots);
}

fn a_reply_of_many_problems_holds_no_more_than_the_bound() {
    let text = "nope \"a\" {}\n".repeat(ENTRIES);
    serve_large(&text, "document.unknown-block");
}

fn a_reply_of_many_changes_holds_no_more_than_the_bound() {
    let placement = "placement \"p\" {\n  select = \"site.*\"\n  home = \"edge\"\n}\n";
    let channels =
        (0..ENTRIES).map(|n| format!("channel \"site.c{n}\" {{ kind = \"index\" }}\n"));
    let text = std::iter::once(placement.to_owned())
        .chain(channels)
        .collect::<String>();
    serve_large(&text, "ops.large-reply");
}

/// The front ends of the node: HCL.
fn hcl() -> FrontEnds {
    FrontEnds::new(
        "hcl",
        FrontEnd {
            read: config_hcl::read,
            update: |source, text, document| {
                config_hcl::update(source, text, document)
                    .map_err(|refusal| Vec::from(&refusal))
            },
        },
    )
}

/// Serves a `plan` request of `text` at a long path, whose reply passes the bound, and
/// asserts that the heap grows by at most the bound and `STEPS_BYTES`, and that the
/// first problem of the reply has `code`.
fn serve_large(text: &str, code: &'static str) {
    let path = format!("{}.hcl", "x".repeat(PATH_BYTES - 4));
    let body = format!(
        "{}\n",
        json!({ "plan": { "files": [{ "path": path, "text": text }] } })
    )
    .into_bytes();
    let mut sim = Sim::new(sim::Config::default());
    let sim_node = sim.node(sim::node::Config::default());
    let ran = sim.run_on(&sim_node, move |sim_node, tasks| async move {
        let definitions = spec::founding::create(ADMIN.public());
        let mesh = create_mesh(&sim_node, &tasks, &["edge"], definitions).await;
        let env = hub::testing::Env {
            files: sim_node.files(),
            clock: sim_node.clock(),
            wall: sim_node.wall(),
            entropy: sim_node.entropy(),
            tasks: tasks.clone(),
        };
        let (hub, _) = hub::testing::open(env).await;
        let secrets = connector::testing::create_secrets(sim_node.files());
        let supervisor = Supervisor::new(supervisor::Config {
            kinds: Arc::new(connector::kind::Table::new()),
            clock: sim_node.clock(),
            entropy: sim_node.entropy(),
            net: sim_node.net(),
            roots: connector::http::Roots::default(),
            tasks,
            hub: hub.clone(),
            secrets,
        });
        let node = Node::new(Config {
            mesh,
            keys: Box::new(|| Key::from_u128(1)),
            front_ends: hcl(),
            supervisor,
            hub,
            clock: sim_node.clock(),
            name: "edge".parse().expect("a name"),
            roots: ops::roots::Failures::new(String::new(), Vec::new()),
        });
        let subject = "@admin".parse().expect("a name");
        let before = ALLOCATOR.held();
        ALLOCATOR.reset_peak();
        let reply = serve(&node, &subject, &body).await;
        let grown = ALLOCATOR.peak() - before;
        let bound = usize::try_from(BODY_BYTES_MAX).expect("the bound fits");
        assert!(
            grown <= bound + STEPS_BYTES,
            "a reply of at most {bound} bytes grew the heap by {grown}",
        );
        let end = reply
            .iter()
            .position(|byte| *byte == b'\n')
            .expect("a line");
        let head: Value = serde_json::from_slice(&reply[..end]).expect("JSON");
        assert_eq!(
            head["errors"][0]["code"], code,
            "the code of the first problem"
        );
    });
    assert_eq!(ran, Ok(()), "the run ends");
}
