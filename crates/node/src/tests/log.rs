use std::io;
use std::panic;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::Duration;

use proptest::prelude::*;
use serde_json::{Value, json};
use types::byte::Size;
use types::time::Span;

use super::{config, heap};
use crate::{Config, Error, Flush, Log, Node, Region, log};

/// The stamp of a line at the start of the sim.
const START: &str = "2026-01-01T00:00:00.000000Z";

/// The variable that marks a child process of a test.
const CHILD: &str = "NODE_LOG_CHILD";

/// The lines a log writes, which each clone shares.
#[derive(Clone, Default)]
struct Lines(Arc<Mutex<Vec<u8>>>);

impl Lines {
    /// Each line written, without its newline.
    fn read(&self) -> Vec<String> {
        let bytes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let text = String::from_utf8(bytes.clone()).expect("a log writes UTF-8");
        text.lines().map(str::to_owned).collect()
    }
}

impl io::Write for Lines {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut lines = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        lines.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A writer whose each write fails.
struct Broken;

impl io::Write for Broken {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("the pipe broke"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A writer that panics on each line that holds `boom`.
struct Panics(Lines);

impl io::Write for Panics {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        assert!(
            !bytes.windows(4).any(|word| word == b"boom"),
            "the writer panics"
        );
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A writer to stderr that, at the line `n=2`, sends on `entered`, waits for the hook
/// on `hooked`, and panics 100 ms later, so the hook waits when it panics.
struct Ends {
    entered: mpsc::Sender<()>,
    hooked: mpsc::Receiver<()>,
}

impl io::Write for Ends {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.windows(3).any(|word| word == b"n=2") {
            self.entered.send(()).unwrap_or_default();
            #[expect(
                clippy::disallowed_methods,
                reason = "the writer panics in the wait of the hook"
            )]
            {
                self.hooked.recv().unwrap_or_default();
                std::thread::sleep(Duration::from_millis(100));
            }
            panic!("the writer panics");
        }
        io::stderr().write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A writer to stderr whose first write blocks until `free` gets a message or drops.
/// It sends on `entered` when it blocks.
struct Gated {
    entered: mpsc::Sender<()>,
    free: Option<mpsc::Receiver<()>>,
}

impl io::Write for Gated {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(free) = self.free.take() {
            self.entered.send(()).unwrap_or_default();
            #[expect(
                clippy::disallowed_methods,
                reason = "a writer blocks the thread log"
            )]
            free.recv().unwrap_or_default();
        }
        io::stderr().write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A writer whose first write blocks until `free` gets a message or drops, as a
/// reader of stderr that stops. It sends on `entered` when it blocks.
struct Stuck {
    lines: Lines,
    entered: Option<mpsc::Sender<()>>,
    free: mpsc::Receiver<()>,
}

impl io::Write for Stuck {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if let Some(entered) = self.entered.take() {
            entered.send(()).unwrap_or_default();
            #[expect(clippy::disallowed_methods, reason = "a writer that blocks")]
            self.free.recv().unwrap_or_default();
        }
        self.lines.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The log of `json` lines of `level` and above to `lines`.
fn to(lines: &Lines, json: bool, level: tracing::Level) -> Log {
    Log {
        writer: Box::new(lines.clone()),
        format: ops::Log { json, level },
    }
}

/// Starts a node on `host` with `log`, runs `sim` for a second, then stops the node
/// and gives its join, once the log has written `stopped`.
fn run(sim: &mut sim::Sim, host: &sim::node::Node, log: Log) -> Result<(), Error> {
    let node = Node::start(Config {
        log,
        ..config(host, Size::MEBIBYTE, Box::new(heap))
    });
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    node.stop();
    assert_eq!(sim.run(), Ok(()));
    let joined = node.join();
    assert_eq!(sim.run(), Ok(()));
    joined
}

/// A sim of `seed` with one host.
fn host(seed: u64) -> (sim::Sim, sim::node::Node) {
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let host = sim.node(sim::node::Config::default());
    (sim, host)
}

#[test]
fn a_node_logs_its_start_and_its_stop_in_text() {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let log = to(&lines, false, tracing::Level::INFO);
    assert_eq!(run(&mut sim, &host, log), Ok(()));
    assert_eq!(
        lines.read(),
        [
            "2026-01-01T00:00:00.004235Z INFO node: started name=edge \
             address=10.0.0.1:7000 shards=4",
            "2026-01-01T00:00:01.000048Z INFO node: stopped",
        ]
    );
}

#[test]
fn a_node_logs_its_start_and_its_stop_in_json() {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let log = to(&lines, true, tracing::Level::INFO);
    assert_eq!(run(&mut sim, &host, log), Ok(()));
    assert_eq!(
        lines.read(),
        [
            concat!(
                r#"{"timestamp":"2026-01-01T00:00:00.004235Z","level":"INFO","#,
                r#""target":"node","fields":{"message":"started","name":"edge","#,
                r#""address":"10.0.0.1:7000","shards":4}}"#,
            ),
            concat!(
                r#"{"timestamp":"2026-01-01T00:00:01.000048Z","level":"INFO","#,
                r#""target":"node","fields":{"message":"stopped"}}"#,
            ),
        ]
    );
}

/// The warnings that a node with no region on `roots` logs in a run of a second.
fn warned(roots: os::roots::Roots) -> Vec<String> {
    logged(roots, tracing::Level::WARN)
}

/// The lines at `level` or above that a node with no region on `roots` logs in a run
/// of a second.
fn logged(roots: os::roots::Roots, level: tracing::Level) -> Vec<String> {
    logged_in(Region::Absent, roots, level)
}

/// The lines at `level` or above that a node with `region` on `roots` logs in a run of
/// a second.
fn logged_in(
    region: Region,
    roots: os::roots::Roots,
    level: tracing::Level,
) -> Vec<String> {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let log = to(&lines, false, level);
    let node = Node::start(Config {
        region,
        roots,
        log,
        ..config(&host, Size::MEBIBYTE, Box::new(heap))
    });
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    node.stop();
    assert_eq!(sim.run(), Ok(()));
    assert_eq!(node.join(), Ok(()));
    lines.read()
}

/// The start of each warning of the roots.
const WARN: &str = "2026-01-01T00:00:00.004235Z WARN node::trust:";
const SKIPPED: &str = "2026-01-01T00:00:00.004235Z WARN node::trust: OS roots that do \
                       not parse are skipped skipped=1";
const NONE: &str = "2026-01-01T00:00:00.004235Z WARN node::trust: the OS roots hold no \
                    certificate, so an https connect with no CA of its connector fails";

#[test]
fn a_node_warns_of_a_failure_of_its_roots_and_of_a_skipped_one() {
    let roots = os::roots::Roots {
        certificates: vec![super::CA.to_vec(), b"not DER".to_vec()],
        failures: vec!["failed to read PEM from file at '/roots.pem'".into()],
        ..os::roots::Roots::default()
    };
    assert_eq!(
        warned(roots),
        [
            summary(1, "the OS store"),
            format!(
                "{WARN} a part of the OS roots was not read error=\"failed to read PEM \
                 from file at '/roots.pem'\""
            ),
            SKIPPED.into(),
        ]
    );
}

#[test]
fn a_node_warns_of_its_roots_before_it_starts() {
    let lines = logged(os::roots::Roots::default(), tracing::Level::INFO);
    let warned = lines.iter().position(|line| line == NONE);
    let started = lines
        .iter()
        .position(|line| line.contains(" node: started "));
    assert!(
        warned.is_some() && warned < started,
        "the warning before `started`: {lines:?}"
    );
}

/// The first warning of a read of the roots with `failed` failures from `source`, on a
/// node with no region.
fn summary(failed: usize, source: &str) -> String {
    format!(
        "{WARN} parts of the OS roots were not read failed={failed} source=\"{source}\""
    )
}

#[test]
fn a_node_with_a_region_names_the_command_that_gives_each_failure_of_its_roots() {
    let warn = "2026-01-01T00:00:00.006100Z WARN node::trust:";
    assert_eq!(
        logged_in(Region::Stored, failed(1), tracing::Level::WARN),
        [
            format!(
                "{warn} parts of the OS roots were not read, and `foundation status` \
                 gives each failed=1 source=\"the OS store\""
            ),
            format!("{warn} a part of the OS roots was not read error=\"failure 0\""),
        ]
    );
}

/// The roots with only the CA and `failures`.
fn failing(failures: Vec<String>) -> os::roots::Roots {
    os::roots::Roots {
        certificates: vec![super::CA.to_vec()],
        failures,
        source: os::roots::Source::default(),
    }
}

/// The roots with only the CA and `count` failures.
fn failed(count: usize) -> os::roots::Roots {
    failing((0..count).map(|n| format!("failure {n}")).collect())
}

/// The warnings of the first `count` failures of `failed`.
fn listed(count: usize) -> Vec<String> {
    (0..count)
        .map(|n| {
            format!("{WARN} a part of the OS roots was not read error=\"failure {n}\"")
        })
        .collect()
}

#[test]
fn a_node_warns_of_the_count_and_the_first_16_failures_of_its_roots() {
    let mut expected = vec![summary(17, "the OS store")];
    expected.extend(listed(16));
    assert_eq!(warned(failed(17)), expected);
}

#[test]
fn one_failure_of_the_roots_gives_the_paths_that_were_read_once() {
    let roots = os::roots::Roots {
        source: os::roots::Source {
            file: Some("/roots.pem".into()),
            dirs: vec!["/a".into(), "/b".into()],
        },
        ..failed(1)
    };
    let mut expected = vec![summary(1, "SSL_CERT_FILE=/roots.pem SSL_CERT_DIR=/a:/b")];
    expected.extend(listed(1));
    assert_eq!(warned(roots), expected);
}

#[test]
fn a_long_source_of_the_roots_is_cut() {
    let dir = format!("/{}", long(3000));
    let roots = os::roots::Roots {
        source: os::roots::Source {
            file: None,
            dirs: vec![dir.clone().into()],
        },
        ..failed(17)
    };
    let source = format!("SSL_CERT_DIR={dir}");
    let (head, tail) = (&source[..510], &source[source.len() - 510..]);
    assert_eq!(
        warned(roots).first(),
        Some(&summary(17, &format!("{head}…{tail}")))
    );
}

#[test]
fn a_node_with_16_failures_of_its_roots_warns_of_each() {
    let mut expected = vec![summary(16, "the OS store")];
    expected.extend(listed(16));
    assert_eq!(warned(failed(16)), expected);
}

/// Whether `lines` hold `started`.
fn started(lines: &[String]) -> bool {
    lines.iter().any(|line| line.contains(" node: started "))
}

#[test]
fn many_roots_failures_keep_started() {
    let lines = logged(failed(1100), tracing::Level::INFO);
    assert!(started(&lines), "`started` in the log: {lines:?}");
}

/// A failure of `bytes`, as a PEM line of that length gives.
fn long(bytes: usize) -> String {
    "x".repeat(bytes)
}

#[test]
fn a_long_failure_of_the_roots_warns_with_its_head_and_its_tail() {
    let failure = format!("head {} tail", long(3000));
    let lines = warned(failing(vec![failure.clone()]));
    let (head, tail) = (&failure[..510], &failure[failure.len() - 510..]);
    assert_eq!(
        lines,
        [
            summary(1, "the OS store"),
            format!(
                "{WARN} a part of the OS roots was not read error=\"{head}…{tail}\""
            ),
        ]
    );
}

#[test]
fn a_failure_of_the_roots_of_1024_bytes_warns_whole_and_one_of_1025_is_cut() {
    let whole = long(1024);
    assert_eq!(
        warned(failing(vec![whole.clone()])),
        [
            summary(1, "the OS store"),
            format!("{WARN} a part of the OS roots was not read error={whole}"),
        ]
    );
    let failure = long(1025);
    let (head, tail) = (&failure[..510], &failure[failure.len() - 510..]);
    assert_eq!(
        warned(failing(vec![failure.clone()])),
        [
            summary(1, "the OS store"),
            format!("{WARN} a part of the OS roots was not read error={head}…{tail}"),
        ]
    );
}

#[test]
fn a_long_failure_of_the_roots_is_cut_at_char_boundaries() {
    let failure = format!("a{}b", "é".repeat(1000));
    let (head, tail) = (&failure[..509], &failure[failure.len() - 509..]);
    assert_eq!(
        warned(failing(vec![failure.clone()])),
        [
            summary(1, "the OS store"),
            format!("{WARN} a part of the OS roots was not read error={head}…{tail}"),
        ]
    );
}

#[test]
fn a_failure_of_the_roots_larger_than_the_log_queue_keeps_started() {
    let lines = logged(failing(vec![long(1200 * 1024)]), tracing::Level::INFO);
    assert!(started(&lines), "`started` in the log: {lines:?}");
}

#[test]
fn sixteen_failures_of_the_roots_larger_than_the_log_queue_keep_started() {
    let lines = logged(failing(vec![long(72 * 1024); 16]), tracing::Level::INFO);
    assert!(started(&lines), "`started` in the log: {lines:?}");
}

#[test]
fn failures_of_the_roots_whose_escapes_grow_them_keep_started() {
    let path = "\u{1}".repeat(131_000);
    let failure = format!("failed to open file: File name too long at '/{path}'");
    let lines = logged(failing(vec![failure; 16]), tracing::Level::INFO);
    assert!(started(&lines), "`started` in the log: {lines:?}");
}

#[test]
fn a_node_with_no_root_warns_that_an_https_connect_with_no_ca_fails() {
    assert_eq!(warned(os::roots::Roots::default()), [NONE]);
    let roots = os::roots::Roots {
        certificates: vec![b"not DER".to_vec()],
        ..os::roots::Roots::default()
    };
    assert_eq!(warned(roots), [SKIPPED, NONE]);
}

#[test]
fn a_node_that_fails_at_its_start_logs_the_error_with_no_mesh_time() {
    let (mut sim, host) = host(1);
    let error = "the name @edge has a segment that starts with `@`, which is reserved \
                 for Foundation; give another name";
    let text = format!(
        "2026-01-01T00:00:00.000000Z ERROR node: stopped error=\"{error}\" \
         mesh_time=none"
    );
    let json = format!(
        concat!(
            r#"{{"timestamp":"2026-01-01T00:00:00.000000Z","level":"ERROR","#,
            r#""target":"node","fields":{{"message":"stopped","error":"{error}"}},"#,
            r#""mesh_time":null}}"#,
        ),
        error = error,
    );
    for (json, line) in [(false, text), (true, json)] {
        let lines = Lines::default();
        let node = Node::start(Config {
            name: "@edge".parse().expect("a name"),
            log: to(&lines, json, tracing::Level::INFO),
            ..config(&host, Size::MEBIBYTE, Box::new(heap))
        });
        assert!(matches!(node.join(), Err(Error::Reserved { .. })));
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(lines.read(), [line], "json: {json}");
    }
}

#[test]
fn a_wall_step_shows_mesh_time_beside_the_stamp() {
    let text = [
        "2026-01-01T00:00:00.004235Z INFO node: started name=edge \
         address=10.0.0.1:7000 shards=4",
        "2026-01-01T00:00:03.000048Z INFO node: stopped \
         mesh_time=2026-01-01T00:00:01.000048Z",
    ];
    let json = [
        concat!(
            r#"{"timestamp":"2026-01-01T00:00:00.004235Z","level":"INFO","#,
            r#""target":"node","fields":{"message":"started","name":"edge","#,
            r#""address":"10.0.0.1:7000","shards":4}}"#,
        ),
        concat!(
            r#"{"timestamp":"2026-01-01T00:00:03.000048Z","level":"INFO","#,
            r#""target":"node","fields":{"message":"stopped"},"#,
            r#""mesh_time":"2026-01-01T00:00:01.000048Z"}"#,
        ),
    ];
    for (json, written) in [(false, text), (true, json)] {
        let (mut sim, host) = host(1);
        let lines = Lines::default();
        let node = Node::start(Config {
            log: to(&lines, json, tracing::Level::INFO),
            ..config(&host, Size::MEBIBYTE, Box::new(heap))
        });
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        host.step_wall(Span::from_nanos(2_000_000_000));
        node.stop();
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(node.join(), Ok(()));
        assert_eq!(sim.run(), Ok(()));
        assert_eq!(lines.read(), written, "json: {json}");
    }
}

#[test]
fn each_node_writes_only_its_own_log() {
    let (mut sim, a) = host(1);
    let b = sim.node(sim::node::Config::default());
    let (to_a, to_b) = (Lines::default(), Lines::default());
    let start = |host: &sim::node::Node, name: &str, lines: &Lines| {
        Node::start(Config {
            name: name.parse().expect("a name"),
            log: to(lines, false, tracing::Level::INFO),
            ..config(host, Size::MEBIBYTE, Box::new(heap))
        })
    };
    let nodes = [start(&a, "a", &to_a), start(&b, "b", &to_b)];
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    for node in &nodes {
        node.stop();
    }
    assert_eq!(sim.run(), Ok(()));
    for node in nodes {
        assert_eq!(node.join(), Ok(()));
    }
    assert_eq!(sim.run(), Ok(()));
    let started = |lines: &Lines| {
        let lines = lines.read();
        let line = lines.first().expect("a started line").clone();
        line.split_once(" INFO ").expect("a level").1.to_owned()
    };
    assert_eq!(
        (started(&to_a), started(&to_b)),
        (
            "node: started name=a address=10.0.0.1:7000 shards=4".to_owned(),
            "node: started name=b address=10.0.0.2:7000 shards=4".to_owned(),
        )
    );
}

#[test]
fn a_level_above_info_drops_the_start_and_the_stop() {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let log = to(&lines, false, tracing::Level::WARN);
    assert_eq!(run(&mut sim, &host, log), Ok(()));
    assert_eq!(lines.read(), Vec::<String>::new());
}

#[test]
fn a_log_whose_writes_fail_changes_nothing_of_the_node() {
    let (mut sim, host) = host(1);
    let log = Log {
        writer: Box::new(Broken),
        format: ops::Log {
            json: false,
            level: tracing::Level::TRACE,
        },
    };
    assert_eq!(run(&mut sim, &host, log), Ok(()));
}

/// The log of `json` lines at INFO to `writer` on `host`, whose mesh clock has not
/// synced, with no thread `log` yet.
fn opened(
    host: &sim::node::Node,
    json: bool,
    writer: Box<dyn io::Write + Send>,
) -> Flush {
    let (_, mesh) = clock::Clock::new(host.clock());
    let format = ops::Log {
        json,
        level: tracing::Level::INFO,
    };
    log::open(Log { writer, format }, &host.threads(), host.wall(), mesh)
}

/// Puts the lines of `events` in the queue of `log`, then writes them in `sim`.
fn write(sim: &mut sim::Sim, log: &Flush, events: impl FnOnce()) {
    tracing::dispatcher::with_default(&log.dispatch, events);
    assert!(log.resume(), "the thread log starts");
    assert_eq!(sim.run(), Ok(()));
}

/// What a log of `json` lines writes for the events of `events`, on a host whose
/// mesh clock has not synced.
fn written(json: bool, events: impl FnOnce()) -> String {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let log = opened(&host, json, Box::new(lines.clone()));
    write(&mut sim, &log, events);
    let bytes = lines.0.lock().unwrap_or_else(PoisonError::into_inner);
    String::from_utf8(bytes.clone()).expect("a log writes UTF-8")
}

/// The events `test` with the field `n` for each of `from..to`.
fn numbered(from: u64, to: u64) -> impl FnOnce() {
    move || {
        for n in from..to {
            tracing::info!(target: "test", n);
        }
    }
}

/// The line of the event `n` of [`numbered`] at the start of the sim.
fn number(n: u64) -> String {
    format!("2026-01-01T00:00:00.000000Z INFO test: n={n} mesh_time=none")
}

#[test]
fn a_full_queue_drops_each_new_line_and_counts_them_after_the_lines_before() {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let log = opened(&host, false, Box::new(lines.clone()));
    write(&mut sim, &log, numbered(0, 1027));
    write(&mut sim, &log, numbered(1027, 1028));
    let mut written: Vec<String> = (0..1024).map(number).collect();
    written.push(
        "2026-01-01T00:00:00.000000Z WARN node::log: dropped lines=3 mesh_time=none"
            .to_owned(),
    );
    written.push(number(1027));
    assert_eq!(lines.read(), written);
}

#[test]
fn a_write_that_panics_ends_the_thread_log_after_the_lines_before() {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let log = opened(&host, false, Box::new(Panics(lines.clone())));
    tracing::dispatcher::with_default(&log.dispatch, || {
        tracing::info!(target: "test", "a");
        tracing::info!(target: "test", "boom");
        tracing::info!(target: "test", "c");
    });
    assert!(log.resume(), "the thread log starts");
    assert_eq!(
        sim.run(),
        Err(sim::Error::Panicked {
            thread: "log".to_owned(),
            message: "the writer panics".to_owned(),
            seed: 1,
        })
    );
    assert!(log.resume(), "no thread log starts, and none fails to");
    assert_eq!(sim.run(), Ok(()));
    assert_eq!(
        lines.read(),
        ["2026-01-01T00:00:00.000000Z INFO test: a mesh_time=none"]
    );
}

#[test]
fn a_line_that_does_not_fit_in_the_bytes_of_the_queue_drops_with_each_line_after_it() {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let log = opened(&host, false, Box::new(lines.clone()));
    let text = "x".repeat(600 << 10);
    write(&mut sim, &log, || {
        tracing::info!(target: "test", text);
        tracing::info!(target: "test", text);
        tracing::info!(target: "test", n = 2);
    });
    write(&mut sim, &log, numbered(3, 4));
    assert_eq!(
        lines.read(),
        [
            format!("{START} INFO test: text={text} mesh_time=none"),
            "2026-01-01T00:00:00.000000Z WARN node::log: dropped lines=2 mesh_time=none"
                .to_owned(),
            number(3),
        ]
    );
}

#[test]
fn a_drop_with_no_thread_log_starts_one_for_its_count_line() {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let log = opened(&host, false, Box::new(lines.clone()));
    let text = "x".repeat(1 << 20);
    tracing::dispatcher::with_default(&log.dispatch, || {
        tracing::info!(target: "test", text);
        tracing::info!(target: "test", n = 1);
    });
    assert!(
        !log.wait(Duration::ZERO),
        "the count line waits for the thread log"
    );
    assert_eq!(sim.run(), Ok(()));
    assert!(
        log.wait(Duration::ZERO),
        "the thread log wrote the count line"
    );
    assert_eq!(
        lines.read(),
        ["2026-01-01T00:00:00.000000Z WARN node::log: dropped lines=2 mesh_time=none"]
    );
}

#[test]
fn a_line_too_large_for_the_queue_wakes_the_thread_log_for_its_count_line() {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let log = opened(&host, false, Box::new(lines.clone()));
    let hold = log.hold().expect("the thread log starts");
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    let text = "x".repeat(1 << 20);
    let large = || tracing::info!(target: "test", text);
    tracing::dispatcher::with_default(&log.dispatch, large);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    assert_eq!(
        unstamped(&lines.read()),
        ["WARN node::log: dropped lines=1 mesh_time=none"]
    );
    drop(hold);
    assert_eq!(sim.run(), Ok(()));
}

#[test]
fn a_resume_while_the_thread_log_runs_starts_no_other() {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let log = opened(&host, false, Box::new(lines.clone()));
    let hold = log.hold().expect("the thread log starts");
    assert!(log.resume(), "the thread log runs");
    tracing::dispatcher::with_default(&log.dispatch, numbered(0, 1));
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    tracing::dispatcher::with_default(&log.dispatch, numbered(1, 2));
    drop(hold);
    assert_eq!(sim.run(), Ok(()));
    assert_eq!(
        unstamped(&lines.read()),
        [
            "INFO test: n=0 mesh_time=none",
            "INFO test: n=1 mesh_time=none"
        ]
    );
}

#[test]
fn a_wait_after_the_hold_drops_waits_for_the_thread_log_to_end_then_joins_it() {
    let (mut sim, host) = host(1);
    let log = opened(&host, false, Box::new(Lines::default()));
    let hold = log.hold().expect("the thread log starts");
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    drop(hold);
    assert!(!log.wait(Duration::ZERO), "the thread log runs");
    assert_eq!(sim.run(), Ok(()));
    assert!(log.wait(Duration::ZERO), "the thread log has ended");
    // A join leaves no trace other than the empty slot of the handle.
    let debug = format!("{log:?}");
    assert!(debug.contains("running: false"), "{debug}");
    assert!(
        debug.contains("handle: None"),
        "the wait joins the thread: {debug}"
    );
}

#[test]
fn a_json_count_line_gives_the_count_as_a_number() {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let log = opened(&host, true, Box::new(lines.clone()));
    write(&mut sim, &log, numbered(0, 1025));
    let read = lines.read();
    let count: Value = serde_json::from_str(&read[1024]).expect("JSON");
    assert_eq!(
        count,
        json!({
            "timestamp": "2026-01-01T00:00:00.000000Z",
            "level": "WARN",
            "target": "node::log",
            "fields": { "message": "dropped", "lines": 1 },
            "mesh_time": null,
        })
    );
    assert_eq!(read.len(), 1025);
}

#[test]
fn a_line_after_the_shards_end_comes_before_stopped() {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let node = Node::start(Config {
        log: to(&lines, false, tracing::Level::INFO),
        ..config(&host, Size::MEBIBYTE, Box::new(heap))
    });
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    node.stop();
    assert_eq!(sim.run(), Ok(()));
    let late = || tracing::info!(target: "test", "late");
    tracing::dispatcher::with_default(&node.flush().dispatch, late);
    assert_eq!(sim.run(), Ok(()));
    assert_eq!(lines.read().len(), 1, "the line waits for the join");
    assert_eq!(node.join(), Ok(()));
    assert_eq!(sim.run(), Ok(()));
    assert_eq!(
        lines.read(),
        [
            "2026-01-01T00:00:00.004235Z INFO node: started name=edge \
             address=10.0.0.1:7000 shards=4",
            "2026-01-01T00:00:01.000048Z INFO test: late",
            "2026-01-01T00:00:01.000048Z INFO node: stopped",
        ]
    );
}

#[test]
fn a_node_whose_thread_log_cannot_start_fails_its_start() {
    let (mut sim, host) = host(1);
    host.fail_thread("log");
    let lines = Lines::default();
    let node = Node::start(Config {
        log: to(&lines, false, tracing::Level::INFO),
        ..config(&host, Size::MEBIBYTE, Box::new(heap))
    });
    let e = node.join().unwrap_err();
    assert_eq!(
        e,
        Error::Start(env::thread::Error::Start {
            name: "log".into(),
            reason: "injected".into()
        })
    );
    assert_eq!(e.to_string(), "cannot start thread log: injected");
    assert_eq!(sim.run(), Ok(()));
    assert!(host.shard_starts().is_empty(), "no shard starts");
    assert_eq!(
        lines.read(),
        [
            "2026-01-01T00:00:00.000000Z ERROR node: stopped error=\"cannot start \
             thread log: injected\" mesh_time=none"
        ]
    );
}

#[test]
fn a_wait_after_the_log_has_ended_with_no_line_starts_no_thread_log() {
    let (mut sim, host) = host(1);
    let node = Node::start(config(&host, Size::MEBIBYTE, Box::new(heap)));
    let log = node.flush();
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    node.stop();
    assert_eq!(sim.run(), Ok(()));
    assert_eq!(node.join(), Ok(()));
    assert_eq!(sim.run(), Ok(()));
    host.fail_thread("log");
    assert!(log.wait(Duration::ZERO), "the wait starts no thread log");
}

#[test]
fn a_thread_log_that_cannot_start_at_the_join_keeps_its_lines_for_the_wait() {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let node = Node::start(Config {
        log: to(&lines, false, tracing::Level::INFO),
        ..config(&host, Size::MEBIBYTE, Box::new(heap))
    });
    let log = node.flush();
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    node.stop();
    assert_eq!(sim.run(), Ok(()));
    host.fail_thread("log");
    assert_eq!(node.join(), Ok(()));
    assert_eq!(sim.run(), Ok(()));
    assert_eq!(lines.read().len(), 1, "`stopped` waits for the wait");
    assert!(!log.wait(Duration::ZERO), "the thread log runs");
    assert_eq!(sim.run(), Ok(()));
    assert!(log.wait(Duration::ZERO), "the thread log wrote `stopped`");
    assert_eq!(
        lines.read(),
        [
            "2026-01-01T00:00:00.004235Z INFO node: started name=edge \
             address=10.0.0.1:7000 shards=4",
            "2026-01-01T00:00:01.000048Z INFO node: stopped",
        ]
    );
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test bounds a real-time wait"
)]
fn a_wait_whose_thread_log_cannot_start_ends_at_once() {
    let (mut sim, host) = host(1);
    let lines = Lines::default();
    let log = opened(&host, false, Box::new(lines.clone()));
    tracing::dispatcher::with_default(&log.dispatch, numbered(0, 1));
    host.fail_thread("log");
    let start = std::time::Instant::now();
    assert!(
        !log.wait(Duration::from_secs(30)),
        "the thread cannot start"
    );
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
    assert!(!log.wait(Duration::ZERO), "the next wait starts the thread");
    assert_eq!(sim.run(), Ok(()));
    assert_eq!(lines.read(), [number(0)]);
}

/// The lines of `lines` with no stamp.
fn unstamped<'a>(lines: &'a [String]) -> Vec<&'a str> {
    let unstamp = |line: &'a String| line.split_once(' ').expect("a stamp").1;
    lines.iter().map(unstamp).collect()
}

/// The log of a writer that blocks at its first line, on OS threads, with its thread
/// `log` running. Gives the log, its hold, what the writer wrote, the receiver of
/// the block of the writer, and the sender that frees it.
fn stuck() -> (
    Flush,
    log::Hold,
    Lines,
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
) {
    let lines = Lines::default();
    let (entered, blocked) = mpsc::channel();
    let (free, freed) = mpsc::channel();
    let writer = Stuck {
        lines: lines.clone(),
        entered: Some(entered),
        free: freed,
    };
    let format = ops::Log {
        json: false,
        level: tracing::Level::INFO,
    };
    let (_, mesh) = clock::Clock::new(os::clock());
    let threads = os::threads().expect("threads");
    let wall = os::wall().expect("a wall clock");
    let log = Log {
        writer: Box::new(writer),
        format,
    };
    let log = log::open(log, &threads, wall, mesh);
    let hold = log.hold().expect("the thread log starts");
    (log, hold, lines, blocked, free)
}

#[test]
fn an_event_never_waits_on_a_stuck_writer() {
    let (log, hold, lines, blocked, free) = stuck();
    tracing::dispatcher::with_default(&log.dispatch, numbered(0, 1));
    #[expect(clippy::disallowed_methods, reason = "the test waits for an OS thread")]
    blocked.recv().expect("the writer blocks");
    tracing::dispatcher::with_default(&log.dispatch, numbered(1, 1030));
    assert!(!log.wait(Duration::from_millis(10)));
    free.send(()).expect("the writer waits");
    drop(hold);
    assert!(log.wait(Duration::from_secs(60)));
    let mut written: Vec<String> = (0..1025)
        .map(|n| format!("INFO test: n={n} mesh_time=none"))
        .collect();
    written.push("WARN node::log: dropped lines=5 mesh_time=none".to_owned());
    assert_eq!(unstamped(&lines.read()), written);
}

/// A wait ends at once when the writer panics, well before its bound.
#[test]
#[expect(clippy::disallowed_methods, reason = "the test waits for an OS thread")]
fn a_wait_ends_when_the_writer_panics() {
    let lines = Lines::default();
    let format = ops::Log {
        json: false,
        level: tracing::Level::INFO,
    };
    let (_, mesh) = clock::Clock::new(os::clock());
    let threads = os::threads().expect("threads");
    let wall = os::wall().expect("a wall clock");
    let writer = Box::new(Panics(lines.clone()));
    let log = log::open(Log { writer, format }, &threads, wall, mesh);
    let hold = log.hold().expect("the thread log starts");
    tracing::dispatcher::with_default(&log.dispatch, || {
        tracing::info!(target: "test", "boom");
    });
    let start = std::time::Instant::now();
    assert!(!log.wait(Duration::from_secs(30)), "the writer panicked");
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "{:?}",
        start.elapsed()
    );
    drop(hold);
}

/// The name of the test whose child runs [`panics`].
const PANICS: &str = "tests::log::a_panic_writes_the_queued_lines_then_the_panic";

/// Runs [`panics`] in a child process with `CHILD` set to `case`, and gives its
/// stderr. Asserts that the child aborts when `case` is `abort`, and panics with no
/// abort otherwise.
fn child(case: &str) -> String {
    let output = Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", PANICS, "--nocapture"])
        .env(CHILD, case)
        .output()
        .expect("the child runs");
    let ended = if case == "abort" { None } else { Some(101) };
    assert_eq!(output.status.code(), ended, "the end of the child");
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        const SIGABRT: i32 = 6;
        let signal = (case == "abort").then_some(SIGABRT);
        assert_eq!(
            output.status.signal(),
            signal,
            "the signal that ends the child"
        );
    }
    String::from_utf8(output.stderr).expect("UTF-8")
}

/// The log lines of `stderr` before the line of the hook of the test harness, with
/// no stamp, and the rest.
fn split(stderr: &str) -> (Vec<String>, &str) {
    let (logged, hook) = stderr
        .split_once("\nthread '")
        .expect("the hook of the test harness writes after the log");
    let lines: Vec<String> = logged.lines().map(str::to_owned).collect();
    let lines = unstamped(&lines).into_iter().map(str::to_owned).collect();
    (lines, hook)
}

/// Asserts that `line` is the text panic line of the panic of [`panics`].
#[track_caller]
fn assert_panicked(line: &str, stderr: &str) {
    let at = r#"ERROR node: panicked panic="panicked at crates/node/src/tests/log.rs:"#;
    let panicked = line.strip_prefix(at).expect("the panic line");
    assert!(
        panicked.ends_with(r#":\nthe test panics" mesh_time=none"#),
        "{stderr}"
    );
}

/// A value whose `Display` panics.
struct Boom;

impl std::fmt::Display for Boom {
    fn fmt(&self, _: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        panic!("the field panics");
    }
}

/// What the child of a panic test runs.
#[derive(Clone, Copy)]
enum Case {
    /// The log writes to stderr.
    Free,
    /// The writer blocks.
    Stuck,
    /// The log drops its hold, then panics in an event.
    Late,
    /// The writer panics at one line, and the log waits for the next.
    Writes,
    /// The writer panics while the hook waits for it.
    Ends,
    /// The log writes to stderr once the queue is full.
    Full,
    /// The log writes to stderr, and the hook before aborts, as in a release build.
    Abort,
    /// The log writes JSON lines to stderr.
    Json,
    /// The log writes to a stderr that nothing reads, then drops its hold.
    Hang,
}

/// The writer of the child of `case`. A writer that blocks sends on `entered`, then
/// waits on `free`.
fn writer(
    case: Case,
    entered: mpsc::Sender<()>,
    free: mpsc::Receiver<()>,
) -> Box<dyn io::Write + Send> {
    match case {
        Case::Stuck => Box::new(Stuck {
            lines: Lines::default(),
            entered: Some(entered),
            free,
        }),
        Case::Writes => Box::new(Panics(Lines::default())),
        Case::Ends => Box::new(Ends {
            entered,
            hooked: free,
        }),
        Case::Full => Box::new(Gated {
            entered,
            free: Some(free),
        }),
        Case::Free | Case::Late | Case::Abort | Case::Json | Case::Hang => {
            Box::new(io::stderr())
        }
    }
}

/// The panic hook of the child of `case`: the hook of `log` over the hook before it.
/// For [`Case::Ends`], it first frees the writer on `free`.
fn hook(
    case: Case,
    log: &Flush,
    free: &mpsc::Sender<()>,
) -> Box<dyn Fn(&panic::PanicHookInfo<'_>) + Send + Sync> {
    let previous = match case {
        Case::Abort => Box::new(|info: &panic::PanicHookInfo<'_>| {
            let line = format!("the hook before runs: {info}\n");
            drop(io::Write::write_all(&mut io::stderr(), line.as_bytes()));
            std::process::abort();
        }),
        _ => panic::take_hook(),
    };
    let hook = log.hook(previous, Duration::from_secs(1));
    match case {
        Case::Ends => {
            let hooked = free.clone();
            Box::new(move |info| {
                hooked.send(()).unwrap_or_default();
                hook(info);
            })
        }
        _ => hook,
    }
}

/// In a child process: a log with its thread `log` held and each line of
/// `numbered(0, 3)` in its queue, and the panic hook of `main`, then a panic, as
/// `case` says.
fn panics(case: Case) -> ! {
    let (_, mesh) = clock::Clock::new(os::clock());
    let (free, freed) = mpsc::channel();
    let (entered, blocked) = mpsc::channel();
    let writer = writer(case, entered, freed);
    let format = ops::Log {
        json: matches!(case, Case::Json),
        level: tracing::Level::INFO,
    };
    let threads = os::threads().expect("threads");
    let wall = os::wall().expect("a wall clock");
    let log = log::open(Log { writer, format }, &threads, wall, mesh);
    tracing::dispatcher::with_default(&log.dispatch, numbered(0, 3));
    let hold = log.hold().expect("the thread log starts");
    panic::set_hook(hook(case, &log, &free));
    match case {
        Case::Late => {
            drop(hold);
            tracing::dispatcher::with_default(&log.dispatch, || {
                tracing::info!(target: "test", field = %Boom);
            });
        }
        Case::Writes => {
            tracing::dispatcher::with_default(&log.dispatch, || {
                tracing::info!(target: "test", "boom");
                tracing::info!(target: "test", "c");
            });
            let bound = Duration::from_millis(500);
            assert!(!log.wait(bound), "a line after the panic is written");
        }
        Case::Hang => {
            let text = "x".repeat(4096);
            tracing::dispatcher::with_default(&log.dispatch, || {
                for n in 0..200_u64 {
                    tracing::info!(target: "test", n, text);
                }
            });
            assert!(!log.wait(Duration::from_millis(500)), "the pipe is full");
            drop(hold);
        }
        Case::Ends => {
            #[expect(
                clippy::disallowed_methods,
                reason = "the bound starts after n=2"
            )]
            blocked.recv().expect("the writer reaches n=2");
        }
        Case::Full => {
            #[expect(clippy::disallowed_methods, reason = "the writer blocks first")]
            blocked.recv().expect("the writer blocks");
            tracing::dispatcher::with_default(&log.dispatch, numbered(3, 1100));
            free.send(()).expect("the writer waits");
        }
        Case::Free | Case::Stuck | Case::Abort | Case::Json => {}
    }
    panic!("the test panics");
}

#[test]
fn a_panic_writes_the_queued_lines_then_the_panic() {
    #[expect(clippy::disallowed_methods, reason = "the parent test marks its child")]
    match std::env::var(CHILD).as_deref() {
        Ok("free") => panics(Case::Free),
        Ok("stuck") => panics(Case::Stuck),
        Ok("late") => panics(Case::Late),
        Ok("writes") => panics(Case::Writes),
        Ok("ends") => panics(Case::Ends),
        Ok("full") => panics(Case::Full),
        Ok("abort") => panics(Case::Abort),
        Ok("json") => panics(Case::Json),
        Ok("hang") => panics(Case::Hang),
        _ => {}
    }
    let stderr = child("free");
    let (lines, hook) = split(&stderr);
    let queued: Vec<String> = (0..3)
        .map(|n| format!("INFO test: n={n} mesh_time=none"))
        .collect();
    assert_eq!(lines[..3], queued, "{stderr}");
    assert_panicked(&lines[3], &stderr);
    assert_eq!(lines.len(), 4, "{stderr}");
    assert!(hook.contains("panicked at"), "{stderr}");
}

#[test]
fn a_panic_with_a_full_queue_writes_its_line_after_the_queued_lines() {
    let stderr = child("full");
    let (lines, _) = split(&stderr);
    let (panicked, lines) = lines.split_last().expect("lines");
    assert_panicked(panicked, &stderr);
    let mut numbers = Vec::new();
    let mut dropped = 0;
    for line in lines {
        let field = |prefix| {
            let value = line.strip_prefix(prefix)?.strip_suffix(" mesh_time=none")?;
            value.parse::<usize>().ok()
        };
        match (
            field("INFO test: n="),
            field("WARN node::log: dropped lines="),
        ) {
            (Some(n), None) => numbers.push(n),
            (None, Some(lines)) => dropped += lines,
            _ => panic!("not a line of the test: {line:?}\n{stderr}"),
        }
    }
    assert!(dropped > 0, "the queue was full: {stderr}");
    assert_eq!(numbers.len() + dropped, 1100, "{stderr}");
    assert!(numbers.is_sorted(), "{stderr}");
}

#[test]
fn a_panic_writes_its_line_before_a_hook_before_that_aborts() {
    let stderr = child("abort");
    let (logged, hook) = stderr
        .split_once("the hook before runs: ")
        .expect("the hook before runs");
    let lines: Vec<String> = logged.lines().map(str::to_owned).collect();
    let lines = unstamped(&lines);
    assert_eq!(lines.len(), 4, "{stderr}");
    assert_panicked(lines[3], &stderr);
    assert!(hook.contains("the test panics"), "{stderr}");
}

#[test]
fn a_json_panic_line_gives_the_panic_as_a_field() {
    let stderr = child("json");
    let (logged, _) = stderr
        .split_once("\nthread '")
        .expect("the hook of the test harness writes after the log");
    let last = logged.lines().last().expect("the panic line");
    let mut line: Value = serde_json::from_str(last).expect("JSON");
    let panic = line["fields"]["panic"].take();
    let panic = panic.as_str().expect("the panic text");
    assert!(
        panic.starts_with("panicked at crates/node/src/tests/log.rs:"),
        "{panic}"
    );
    assert!(panic.ends_with(":\nthe test panics"), "{panic}");
    line["timestamp"].take();
    assert_eq!(
        line,
        json!({
            "timestamp": null,
            "level": "ERROR",
            "target": "node",
            "fields": { "message": "panicked", "panic": null },
            "mesh_time": null,
        })
    );
}

#[test]
fn a_panic_with_a_stuck_writer_ends_after_the_bound_with_no_line() {
    let stderr = child("stuck");
    assert!(!stderr.contains("the test panics"), "{stderr}");
}

#[test]
fn a_panic_after_the_hold_drops_with_a_stuck_stderr_ends_after_the_bound() {
    let mut child = Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", PANICS, "--nocapture"])
        .env(CHILD, "hang")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the child runs");
    let mut ended = None;
    for _ in 0..400 {
        if let Some(status) = child.try_wait().expect("a wait") {
            ended = Some(status.code());
            break;
        }
        #[expect(clippy::disallowed_methods, reason = "the test waits for a child")]
        std::thread::sleep(Duration::from_millis(50));
    }
    if ended.is_none() {
        child.kill().expect("a kill");
        child.wait().expect("a wait");
    }
    assert_eq!(
        ended,
        Some(Some(101)),
        "the child ends within 20 s, after the bound of the hook"
    );
}

#[test]
fn a_panic_in_an_event_after_the_shards_end_reaches_stderr_with_no_abort() {
    let stderr = child("late");
    assert!(stderr.contains("the field panics"), "{stderr}");
}

#[test]
fn a_panic_of_the_writer_ends_the_log_and_runs_the_hook_before_at_once() {
    let stderr = child("writes");
    assert!(stderr.contains("the writer panics"), "{stderr}");
    assert!(stderr.contains("the test panics"), "{stderr}");
}

#[test]
fn a_panic_that_waits_as_the_writer_panics_runs_the_hook_before() {
    let stderr = child("ends");
    assert!(stderr.contains("the writer panics"), "{stderr}");
    assert!(stderr.contains(":\nthe test panics\n"), "{stderr}");
}

/// The line of an event with `field` and other fields.
fn line(json: bool, field: &str) -> String {
    written(json, || {
        tracing::info!(
            target: "test",
            field,
            count = 3_u64,
            delta = -2_i64,
            ok = true,
            "a\nb"
        );
    })
}

#[test]
fn a_newline_and_a_backslash_in_a_field_stay_on_one_line() {
    assert_eq!(
        line(false, "x\ny\\z\"q"),
        concat!(
            r#"2026-01-01T00:00:00.000000Z INFO test: a\nb "#,
            r#"field="x\ny\\z\"q" count=3 delta=-2 ok=true mesh_time=none"#,
            "\n",
        )
    );
    assert_eq!(
        line(true, "x\ny\\z\"q"),
        concat!(
            r#"{"timestamp":"2026-01-01T00:00:00.000000Z","level":"INFO","#,
            r#""target":"test","fields":{"message":"a\nb","field":"x\ny\\z\"q","#,
            r#""count":3,"delta":-2,"ok":true},"mesh_time":null}"#,
            "\n",
        )
    );
}

#[test]
fn a_text_value_is_quoted_only_when_it_must_be() {
    for (field, written) in [
        ("plain", "plain"),
        ("", r#""""#),
        ("a b", r#""a b""#),
        ("a=b", r#""a=b""#),
        ("it's", "it's"),
        ("\t", r#""\t""#),
        ("\u{1b}", r#""\u{1b}""#),
    ] {
        let line = written_field(field);
        assert_eq!(line, written, "{field:?}");
    }
}

/// The text of the one field `field` of an event with no message.
fn written_field(field: &str) -> String {
    let line = written(false, || tracing::info!(target: "test", field));
    let rest = line.strip_prefix("2026-01-01T00:00:00.000000Z INFO test: field=");
    let rest = rest.and_then(|rest| rest.strip_suffix(" mesh_time=none\n"));
    rest.unwrap_or_else(|| panic!("not a line of one field: {line:?}"))
        .to_owned()
}

/// The string that `text` escapes as `char::escape_debug` does, for the escapes that
/// the log writes, else `None`.
fn unescape(text: &str) -> Option<String> {
    let mut chars = text.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match chars.next()? {
            '\\' => '\\',
            '"' => '"',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            '0' => '\0',
            'u' => {
                let hex: String =
                    chars.by_ref().skip(1).take_while(|&c| c != '}').collect();
                char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?
            }
            _ => return None,
        });
    }
    Some(out)
}

#[test]
fn a_json_line_gives_a_128_bit_integer_as_a_number() {
    let line = written(true, || {
        tracing::info!(target: "test", big = 5_u128, small = -5_i128);
    });
    let line: Value = serde_json::from_str(&line).expect("JSON");
    assert_eq!(line["fields"], json!({ "big": 5, "small": -5 }));
}

proptest! {
    #[test]
    fn a_text_message_is_one_line_that_reads_back(message in any::<String>()) {
        let line = written(false, || tracing::info!(target: "test", "{message}"));
        let rest = line.strip_prefix("2026-01-01T00:00:00.000000Z INFO test: ");
        let rest = rest.and_then(|rest| rest.strip_suffix(" mesh_time=none\n"));
        let rest = rest.expect("one line");
        prop_assert!(!rest.contains('\n'), "{line:?}");
        prop_assert_eq!(unescape(rest), Some(message));
    }

    #[test]
    fn a_text_value_is_one_value_that_reads_back(field in any::<String>()) {
        let written = written_field(&field);
        prop_assert!(!written.contains('\n'), "{written:?}");
        let quoted = written.strip_prefix('"').and_then(|w| w.strip_suffix('"'));
        let read = if let Some(quoted) = quoted {
            let open = quoted.replace("\\\\", "").replace("\\\"", "");
            prop_assert!(!open.contains('"'), "an unescaped quote: {written:?}");
            unescape(quoted)
        } else {
            prop_assert!(!written.contains([' ', '=', '"', '\\']), "{written:?}");
            Some(written.clone())
        };
        prop_assert_eq!(read, Some(field));
    }

    #[test]
    fn a_json_line_gives_back_each_value(field in any::<String>()) {
        let line = line(true, &field);
        prop_assert_eq!(line.matches('\n').count(), 1);
        let line: Value = serde_json::from_str(&line).expect("JSON");
        prop_assert_eq!(&line["fields"]["field"], &json!(field));
    }
}

#[test]
fn the_debug_of_a_log_hides_its_writer() {
    let (_, mesh) = clock::Clock::new(os::clock());
    let format = ops::Log {
        json: false,
        level: tracing::Level::INFO,
    };
    let writer = Box::new(Lines::default());
    let threads = os::threads().expect("threads");
    let wall = os::wall().expect("a wall clock");
    let log = log::open(Log { writer, format }, &threads, wall, mesh);
    let debug = format!("{log:?}");
    assert!(debug.contains("writer: Some(Writer)"), "{debug}");
}

#[test]
fn the_bench_log_gives_the_dispatch_of_its_level() {
    let (_, mesh) = clock::Clock::new(os::clock());
    let lines = Lines::default();
    let threads = os::threads().expect("threads");
    let wall = os::wall().expect("a wall clock");
    let log = crate::bench::log(
        to(&lines, false, tracing::Level::INFO),
        &threads,
        wall,
        mesh,
    );
    let enabled = tracing::dispatcher::with_default(log.dispatch(), || {
        (
            tracing::enabled!(tracing::Level::INFO),
            tracing::enabled!(tracing::Level::DEBUG),
        )
    });
    assert_eq!(enabled, (true, false));
}

#[test]
fn a_wait_beside_a_resume_joins_the_thread_log() {
    let threads = os::threads().expect("threads");
    for round in 0..500 {
        let lines = Lines::default();
        let (_, mesh) = clock::Clock::new(os::clock());
        let wall = os::wall().expect("a wall clock");
        let log = to(&lines, false, tracing::Level::INFO);
        let log = log::open(log, &threads, wall, mesh);
        tracing::dispatcher::with_default(&log.dispatch, numbered(0, 1));
        let resume = {
            let log = log.clone();
            threads.start("resume", move || async move { assert!(log.resume()) })
        };
        // The resume of the other thread starts the run, so the wait starts none. Only
        // `running` shows that it started, and only the empty slot of the handle shows
        // the join.
        while !format!("{log:?}").contains("running: true") && lines.read().is_empty() {
            std::hint::spin_loop();
        }
        assert!(log.wait(Duration::from_secs(10)), "round {round}");
        resume
            .expect("the thread starts")
            .join()
            .expect("the resume ends");
        assert_eq!(lines.read().len(), 1, "round {round}");
        let debug = format!("{log:?}");
        assert!(debug.contains("handle: None"), "round {round}: {debug}");
    }
}

/// A driver whose start drops its body and panics, as `os` passes the panic of the
/// build of a thread to its caller.
struct Unwinds;

impl env::threads::Driver for Unwinds {
    fn start(
        &self,
        name: &str,
        body: env::threads::Body,
        _: tracing::Dispatch,
    ) -> Result<env::thread::Handle, env::thread::Error> {
        drop(body);
        panic!("the start of {name} panicked");
    }
}

#[test]
fn a_wait_after_a_start_that_panicked_has_nothing_to_wait_for() {
    let threads = env::threads::Threads::new(Unwinds);
    let lines = Lines::default();
    let (_, mesh) = clock::Clock::new(os::clock());
    let wall = os::wall().expect("a wall clock");
    let log = log::open(
        to(&lines, false, tracing::Level::INFO),
        &threads,
        wall,
        mesh,
    );
    let held = panic::catch_unwind(panic::AssertUnwindSafe(|| log.hold()));
    assert!(held.is_err(), "the start panicked");
    let debug = format!("{log:?}");
    assert!(log.wait(Duration::from_secs(1)), "{debug}");
}
