//! `foundation mcp` never panics or fails, and it writes at most one reply for each
//! line it reads.

#![no_main]
#![expect(clippy::disallowed_methods, reason = "fuzz_target! calls File::create")]

use std::path::Path;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use document::diagnostic::Code;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = sim.node(sim::node::Config::default());
    // `foundation mcp` refuses to start at `/` with no `--files`.
    host.make_dir(Path::new("/work"));
    host.set_working_dir(Path::new("/work"));
    let paths = host.paths();
    let front_ends = node::front_ends();
    let kinds = node::kinds();
    // Each request fails at once, as when no node runs.
    let request = async |_: &[u8]| -> Result<Vec<u8>, ops::Failure> {
        Err(ops::Failure {
            code: Code::new("node.not-running"),
            message: "no node runs in foundation-data".to_owned(),
            fix: "Start one".to_owned(),
        })
    };
    let reach = async |_: &Path| Ok(request);
    let mut lines = input;
    let cli = ops::cli(
        ["foundation", "mcp"].map(Into::into),
        async || ops::line(&mut lines),
        &mut output,
        &mut errors,
        &paths,
        &front_ends,
        &kinds,
        reach,
    );
    // No request waits, so one poll ends it.
    let Poll::Ready(status) = pin!(cli).poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("`mcp` waited");
    };
    assert_eq!(
        (status, errors.as_slice()),
        (ops::Run::Exit(0), b"".as_slice())
    );
    let lines = input.split(|&byte| byte == b'\n').count();
    assert!(output.iter().filter(|&&byte| byte == b'\n').count() <= lines);
});
