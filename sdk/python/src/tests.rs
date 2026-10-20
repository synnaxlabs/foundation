use std::env;
use std::ffi::CStr;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use pyo3::exceptions::PyConnectionError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};
use pyo3::wrap_pymodule;
use sdk::write::{End, Outcome, Refusal};
use types::channel;
use types::ed25519::PrivateKey;
use types::frame;
use types::sample::{Scalar, Type};

use crate::profile::Profile;

// Each file runs as a caller whose profile names a subject with no key file, so that
// a test that reads the profile of the caller fails.
const SUITE: &CStr = c"
import io, os, sys, tempfile, unittest
from unittest import mock
sys.modules['foundation._native'] = native
sys.path.insert(0, str(package))
sys.path.insert(0, str(top))
stream = io.StringIO()
suite = unittest.TestLoader().loadTestsFromName(module)
with tempfile.TemporaryDirectory(prefix='foundation-python-') as caller:
    profile = os.path.join(caller, 'profile.hcl')
    with open(profile, 'w') as file:
        file.write('subject = \"caller\"\\nprivate_key = \"caller.key\"\\n')
    with mock.patch.dict(os.environ, {'FOUNDATION_PROFILE': profile}):
        result = unittest.TextTestRunner(stream, verbosity=2).run(suite)
passed = result.wasSuccessful() and result.testsRun > len(result.skipped)
report = stream.getvalue()
";

/// The files of `sdk/python/tests`, each with whether it runs only with the feature
/// `sim`.
const FILES: [(&str, bool); 5] = [
    ("test_stubs.py", false),
    ("test_client.py", false),
    ("test_reader.py", false),
    ("test_sim.py", true),
    ("test_writer.py", false),
];

/// Runs each file of the Python suite `sdk/python/tests` on its own in this process,
/// with this build as `foundation._native`. `FOUNDATION_NODE` is the path of the
/// `foundation` binary that the suite starts. Panics with the report as [`run`] says,
/// and when `FOUNDATION_NODE` is not set.
#[test]
fn the_python_suite_passes() {
    #[expect(clippy::disallowed_methods, reason = "the suite reads the node's path")]
    let node = env::var_os("FOUNDATION_NODE");
    assert!(
        node.is_some(),
        "FOUNDATION_NODE is not set: give it the path of the foundation binary"
    );
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    if let Err(problem) = run(&dir, &FILES, cfg!(feature = "sim")) {
        panic!("{problem}");
    }
}

#[test]
fn a_file_that_runs_no_test_fails() {
    let files = [
        ("more/test_more.py", false),
        ("more/test_none.py", false),
        ("test_some.py", false),
        ("test_none.py", false),
        ("test_skip.py", true),
    ];
    let problem = run(&fixture(), &files, false).unwrap_err();
    assert!(problem.starts_with("test_none.py: "), "{problem}");
    assert!(problem.contains("Ran 0 tests"), "{problem}");
}

#[test]
fn a_file_whose_tests_all_skip_fails() {
    let files = [
        ("more/test_more.py", false),
        ("more/test_none.py", false),
        ("test_some.py", false),
        ("test_skip.py", false),
        ("test_none.py", false),
    ];
    let problem = run(&fixture(), &files, false).unwrap_err();
    assert!(problem.starts_with("test_skip.py: "), "{problem}");
    assert!(problem.contains("skipped=1"), "{problem}");
}

#[test]
fn a_file_of_the_sim_feature_runs_only_with_it() {
    let files = [
        ("more/test_more.py", false),
        ("more/test_none.py", false),
        ("test_none.py", true),
        ("test_skip.py", true),
        ("test_some.py", false),
    ];
    assert_eq!(run(&fixture(), &files, false), Ok(()));
}

#[test]
fn an_unlisted_file_fails() {
    let files = [("test_none.py", false), ("test_some.py", false)];
    assert_eq!(
        run(&fixture(), &files, false),
        Err("the test files of the suite are [\"more/test_more.py\", \
             \"more/test_none.py\", \"test_none.py\", \"test_skip.py\", \
             \"test_some.py\"]: change `FILES` to them"
            .to_owned())
    );
}

/// The directory of the fixture suite.
fn fixture() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixture")
}

/// Runs each of `files`, the `test_*.py` files under `dir` with whether each runs only
/// with the feature `sim`, on its own, in their order. `sim` is whether the feature is
/// on. Returns the report of the first file that fails or runs no test that it does not
/// skip, and an error when `files` are not the `test_*.py` files under `dir`.
fn run(dir: &Path, files: &[(&str, bool)], sim: bool) -> Result<(), String> {
    let mut found = Vec::new();
    tests(dir, "", &mut found);
    found.sort();
    let mut listed: Vec<_> = files.iter().map(|&(file, _)| file).collect();
    listed.sort_unstable();
    if found != listed {
        return Err(format!(
            "the test files of the suite are {found:?}: change `FILES` to them"
        ));
    }
    for &(file, only) in files {
        if sim || !only {
            let (passed, report) = suite(dir, file);
            if !passed {
                return Err(format!("{file}: {report}"));
            }
        }
    }
    Ok(())
}

/// Adds to `found` the path of each `test_*.py` file under `dir`, with `/` between its
/// parts, after `prefix`.
fn tests(dir: &Path, prefix: &str, found: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if path.is_dir() {
            tests(&path, &format!("{prefix}{name}/"), found);
        } else if name.starts_with("test_")
            && path.extension().is_some_and(|e| e == "py")
        {
            found.push(format!("{prefix}{name}"));
        }
    }
}

/// Runs each test of the module of `file`, a `.py` path under `dir`, and gives whether
/// one ran that did not skip and each passed, and the report.
fn suite(dir: &Path, file: &str) -> (bool, String) {
    let package = Path::new(env!("CARGO_MANIFEST_DIR")).join("python");
    let module = file.strip_suffix(".py").unwrap_or(file).replace('/', ".");
    Python::initialize();
    Python::attach(|py| -> PyResult<(bool, String)> {
        let globals = PyDict::new(py);
        globals.set_item("native", wrap_pymodule!(super::_native)(py))?;
        globals.set_item("package", package)?;
        globals.set_item("module", module)?;
        globals.set_item("top", dir)?;
        py.run(SUITE, Some(&globals), None)?;
        Ok((
            globals.as_any().get_item("passed")?.extract()?,
            globals.as_any().get_item("report")?.extract()?,
        ))
    })
    .unwrap_or_else(|e| panic!("the suite did not run: {e}"))
}

/// Maps each variant to its class and text. No test of the suite reaches `Bind`: the
/// bind of an ephemeral port fails only when the process has no free file descriptor,
/// and the count of free ones changes with the process.
#[test]
fn error_raises_the_class_and_text_of_each_variant() {
    let [pinned, proved] = [1, 2].map(|byte| PrivateKey([byte; 32]).public());
    let other = format!("the node proves public key {proved}, not the pinned {pinned}");
    let cases = [
        (
            sdk::Error::Thread {
                reason: "the test".into(),
            },
            "RuntimeError",
            "cannot start the client thread: the test",
        ),
        (
            sdk::Error::Bind(transport::Error::TimedOut),
            "OSError",
            "cannot bind the local UDP socket: the peer stopped answering",
        ),
        (
            sdk::Error::Memory(os::memory::Error::Refused),
            "MemoryError",
            "the OS refused memory for the frames of the client's readers; free memory or \
             raise the commit limit of the system",
        ),
        (
            sdk::Error::Session(program::Error::Unanswered),
            "ConnectionError",
            "the node finished a stream with no answer",
        ),
        (
            sdk::Error::Local(sector::local::Error::NotRunning),
            "ConnectionError",
            "no node runs on the data directory",
        ),
        (
            sdk::Error::Panicked,
            "PanicException",
            "the client thread panicked",
        ),
        (
            sdk::Error::Stream(program::Error::Unanswered),
            "ConnectionError",
            "the node finished a stream with no answer",
        ),
        (
            sdk::Error::Value {
                place: 1,
                scalar: types::sample::Scalar::F64,
                data_type: types::sample::Type::Scalar(types::sample::Scalar::I64),
            },
            "TypeError",
            "place 1 holds i64, not f64",
        ),
        (sdk::Error::Closed, "RuntimeError", "the client is closed"),
        (
            sdk::Error::Session(program::Error::Unplaced("plant.a".parse().unwrap())),
            "ConnectionError",
            "the node gave no place for the channel plant.a of the open",
        ),
        (
            sdk::Error::OtherKey { pinned, proved },
            "ConnectionError",
            &other,
        ),
    ];
    raises(cases);
}

/// Maps each error of a stream that sent nothing and leaves a writer going to
/// `ValueError`.
#[test]
fn error_raises_value_error_for_each_stream_error_that_sent_nothing() {
    let cases = [
        (
            sdk::Error::Stream(program::Error::Layout(frame::Error::OutOfRange {
                entry: 3,
                entries: 2,
            })),
            "ValueError",
            "the series of the draft are not valid: entry 3 is out of range for a key \
             set of 2 entries",
        ),
        (
            sdk::Error::Stream(program::Error::Unsent {
                count: 0,
                form: frame::Form::Raw,
            }),
            "ValueError",
            "the frame or a group of it holds no sample, so it was not sent",
        ),
        (
            sdk::Error::Stream(program::Error::Body { length: 9 }),
            "ValueError",
            "the body has 9 bytes, over the 16777216 that a request holds",
        ),
        (
            sdk::Error::Stream(program::Error::Open { length: 9 }),
            "ValueError",
            "the open has 9 bytes, over the 65536 that an open holds",
        ),
        (
            sdk::Error::Stream(program::Error::Unnamed),
            "ValueError",
            "the stream names no channel",
        ),
    ];
    raises(cases);
}

/// Asserts that `error` maps each variant of `cases` to its class and text.
fn raises<const N: usize>(cases: [(sdk::Error, &str, &str); N]) {
    Python::initialize();
    Python::attach(|py| {
        for (variant, class, text) in cases {
            let error = super::error(py, &variant);
            assert_eq!(error.get_type(py).name().unwrap().to_string(), class);
            assert_eq!(error.value(py).to_string(), text);
        }
    });
}

/// Maps an error of the pool of a reader to `ConnectionError`, as the frames of a
/// reader come from the node, and each other error, a refusal of the stream included,
/// as `error` does.
#[test]
fn raised_raises_connection_error_for_an_error_of_the_pool_of_a_reader() {
    let pool = sdk::Error::Stream(program::Error::Pool(block::Error::TooLarge {
        requested: 9,
        largest: 8,
    }));
    let unknown =
        sdk::Error::Stream(program::Error::Refused(program::Refusal::Unknown));
    let cases = [
        (
            pool,
            "ConnectionError",
            "no block of the pool holds a message or a frame: block of 9 bytes is \
             above the largest block of 8 bytes",
            None,
        ),
        (
            unknown,
            "Stopped",
            "the selector matches no channel, or a channel of the read or write is \
             unknown or was removed",
            Some("unknown"),
        ),
        (
            sdk::Error::Closed,
            "RuntimeError",
            "the client is closed",
            None,
        ),
    ];
    Python::initialize();
    Python::attach(|py| {
        for (variant, class, text, code) in cases {
            let error = super::read::raised(py, &variant);
            assert_eq!(error.get_type(py).name().unwrap().to_string(), class);
            let value = error.value(py);
            assert_eq!(value.to_string(), text);
            let given = value.getattr("code").ok().map(|code| code.to_string());
            assert_eq!(given.as_deref(), code);
        }
    });
}

/// Maps a write that ended with a frame that may not be on disk to `Stopped`, with the
/// code of a stream that the node stopped for the cause of its group's end, and its
/// text.
#[test]
fn error_raises_stopped_with_the_code_of_the_end_of_an_unstored_write() {
    let ends = [
        (program::write::End::Removed, "unknown"),
        (program::write::End::Failed, "failed"),
    ];
    Python::initialize();
    Python::attach(|py| {
        for (end, code) in ends {
            let unstored = program::Error::Unstored(vec![(1, end)]);
            let text = unstored.to_string();
            let error = super::error(py, &sdk::Error::Stream(unstored));
            assert_eq!(error.get_type(py).name().unwrap().to_string(), "Stopped");
            let value = error.value(py);
            assert_eq!(value.to_string(), text);
            let given: String = value.getattr("code").unwrap().extract().unwrap();
            assert_eq!(given, code, "{end:?}");
        }
    });
}

/// The places of a writer of the indexes `plant.time`, `cell.time`, `line.time`, and
/// `other.time`, with one channel of each, the channels first.
fn grouped() -> Vec<sdk::Place> {
    let names = [
        "plant.a",
        "cell.a",
        "line.a",
        "other.a",
        "plant.time",
        "cell.time",
        "line.time",
        "other.time",
    ];
    let scalars = [Scalar::F64, Scalar::Stamp];
    (0..names.len())
        .map(|place| sdk::Place {
            key: channel::Key::from_u128(u128::try_from(place).unwrap() + 1),
            data_type: Type::Scalar(scalars[place / 4]),
            name: names[place].parse().unwrap(),
            index: place % 4 + 4,
        })
        .collect()
}

/// The outcome of a group whose frame got the seqs from `seq`, `count` of them.
fn range(seq: u64, count: u32) -> frame::Range {
    frame::Range { seq, count }
}

/// The class, the text, and the `code`, `written`, and `unwritten` of an exception,
/// when it has them.
type Raised = (
    String,
    String,
    Option<String>,
    Option<(Vec<String>, Vec<String>)>,
);

/// What `raised` raises for a frame of the items at `frame` of a writer of `places`,
/// with `outcomes`.
fn untaken(
    places: &[sdk::Place],
    loss: crate::Loss,
    outcomes: &[(usize, Outcome)],
    frame: &[usize],
) -> Raised {
    Python::initialize();
    Python::attach(|py| {
        let error =
            crate::write::raised(py, places, loss, outcomes, frame).unwrap_err();
        let value = error.value(py);
        let code = value
            .getattr("code")
            .ok()
            .map(|code| code.extract().unwrap());
        let names = |name: &str| {
            let names = value.getattr(name).ok()?;
            assert!(names.is_instance_of::<PyTuple>(), "{name}");
            Some(names.extract::<Vec<String>>().unwrap())
        };
        let written = names("written").zip(names("unwritten"));
        let class = error.get_type(py).name().unwrap().to_string();
        (class, value.to_string(), code, written)
    })
}

/// The names of `names`, owned.
fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|&name| name.to_owned()).collect()
}

/// A frame with an end, a refusal, and a loss raises `PartialWrite` for the end when
/// the node took another group, and its text names the cause of each group that the
/// node did not take.
#[test]
fn raised_names_the_cause_of_each_group_that_the_node_did_not_take() {
    let outcomes = [
        (4, Outcome::Ended(End::Removed)),
        (5, Outcome::Refused(Refusal::Full)),
        (
            6,
            Outcome::Lost {
                range: range(10, 3),
            },
        ),
        (7, Outcome::Taken { range: range(0, 3) }),
    ];
    let raised = untaken(&grouped(), crate::Loss::Raise, &outcomes, &[3, 0, 2, 1]);
    assert_eq!(
        raised,
        (
            "PartialWrite".to_owned(),
            "the write of plant.time ended (unknown: a channel of its group was \
             removed); the node refused the frame of cell.time (full: a backfill frame \
             found no room in the log, or the frame found no block of the node's pool: \
             write it again later); the frame of line.time lost 3 samples; channels \
             plant.a, cell.a, line.a, plant.time, cell.time, line.time were not \
             written; channels other.a, other.time were; each later frame of an ended \
             group raises again, and the other groups go on"
                .to_owned(),
            Some("unknown".to_owned()),
            Some((
                names(&["other.a", "other.time"]),
                names(&[
                    "plant.a",
                    "cell.a",
                    "line.a",
                    "plant.time",
                    "cell.time",
                    "line.time"
                ])
            )),
        )
    );
    Python::attach(|py| {
        let class = py.get_type::<crate::PartialWrite>();
        assert!(class.is_subclass_of::<crate::Stopped>().unwrap());
    });
}

/// With no end, a refusal raises `Rejected` with the code of the first refused group,
/// and an end with no group taken raises `Stopped` with the code of the first end.
#[test]
fn raised_raises_for_an_end_then_a_refusal() {
    let refused = [
        (5, Outcome::Refused(Refusal::Waiting)),
        (6, Outcome::Refused(Refusal::Large)),
        (
            7,
            Outcome::Lost {
                range: range(10, 3),
            },
        ),
    ];
    assert_eq!(
        untaken(&grouped(), crate::Loss::Raise, &refused, &[1, 2, 3]),
        (
            "Rejected".to_owned(),
            "the node refused the frame of cell.time (waiting: another writer holds \
             control); the node refused the frame of line.time (large: the frame is \
             too large for one write: split it); the frame of other.time lost 3 \
             samples; channels cell.a, line.a, other.a, cell.time, line.time, \
             other.time were not written; no channel was"
                .to_owned(),
            Some("waiting".to_owned()),
            Some((
                Vec::new(),
                names(&[
                    "cell.a",
                    "line.a",
                    "other.a",
                    "cell.time",
                    "line.time",
                    "other.time"
                ])
            )),
        )
    );
    let ended = [
        (4, Outcome::Refused(Refusal::Order)),
        (5, Outcome::Ended(End::Failed)),
        (6, Outcome::Ended(End::Refused)),
    ];
    assert_eq!(
        untaken(&grouped(), crate::Loss::Raise, &ended, &[0, 1, 2]),
        (
            "Stopped".to_owned(),
            "the node refused the frame of plant.time (order: a stamp is not after the \
             stamp before it); the write of cell.time ended (failed: the home of its \
             group failed); the write of line.time ended (refused: the home of its \
             group refused the proof of the program); channels plant.a, cell.a, \
             line.a, plant.time, cell.time, line.time were not written; no channel \
             was; each later frame of an ended group raises again, and the other \
             groups go on"
                .to_owned(),
            Some("failed".to_owned()),
            None,
        )
    );
}

/// The end of the group of a writer of one index raises `Stopped` with the code of a
/// stream that the node stopped for the same cause, and says that the writer ended.
#[test]
fn raised_ends_a_writer_of_one_index_at_the_end_of_its_group() {
    let grouped = grouped();
    let places = [0, 4].map(|place| sdk::Place {
        index: 1,
        ..grouped[place].clone()
    });
    let ends = [
        (
            End::Removed,
            "unknown",
            "a channel of its group was removed",
        ),
        (End::Failed, "failed", "the home of its group failed"),
        (
            End::Refused,
            "refused",
            "the home of its group refused the proof of the program",
        ),
    ];
    for (end, code, why) in ends {
        let outcomes = [(1, Outcome::Ended(end))];
        assert_eq!(
            untaken(&places, crate::Loss::Raise, &outcomes, &[0]),
            (
                "Stopped".to_owned(),
                format!(
                    "the write of plant.time ended ({code}: {why}); channels plant.a, \
                     plant.time were not written; no channel was; the writer has ended"
                ),
                Some(code.to_owned()),
                None,
            )
        );
    }
}

/// A loss with no end and no refusal does what the loss says with the channels of the
/// lost groups only, the index of each included.
#[test]
fn raised_gives_the_seqs_of_the_lost_groups_only() {
    let outcomes = [
        (5, Outcome::Taken { range: range(0, 2) }),
        (
            6,
            Outcome::Lost {
                range: range(10, 2),
            },
        ),
        (
            7,
            Outcome::Lost {
                range: range(20, 3),
            },
        ),
    ];
    let places = grouped();
    Python::initialize();
    Python::attach(|py| {
        let raised = crate::write::raised(
            py,
            &places,
            crate::Loss::Raise,
            &outcomes,
            &[7, 3, 1, 2],
        );
        let error = raised.unwrap_err();
        assert_eq!(error.get_type(py).name().unwrap().to_string(), "DataLost");
        let value = error.value(py);
        assert_eq!(
            value.to_string(),
            "samples were lost: line.a (2), other.a (3), line.time (2), other.time (3)"
        );
        let seqs = value.getattr("seqs").unwrap();
        let expected = PyDict::new(py);
        let range = py.get_type::<pyo3::types::PyRange>();
        for (name, start, end) in [
            ("line.a", 10, 12),
            ("other.a", 20, 23),
            ("line.time", 10, 12),
            ("other.time", 20, 23),
        ] {
            expected
                .set_item(name, range.call1((start, end)).unwrap())
                .unwrap();
        }
        assert!(seqs.eq(expected).unwrap(), "{seqs}");
        crate::write::raised(py, &places, crate::Loss::Ignore, &outcomes, &[2, 3])
            .unwrap();
    });
}

/// The text of a close names the index and the end of each group that ended with a
/// frame that may not be on disk.
#[test]
fn unstored_names_the_index_and_the_end_of_each_group() {
    let places = grouped();
    assert_eq!(
        crate::write::unstored(&places, &[(4, End::Removed)]),
        "the write of plant.time (unknown) ended: a frame that the node took may not \
         be on disk"
    );
    assert_eq!(
        crate::write::unstored(&places, &[(4, End::Removed), (6, End::Failed)]),
        "the writes of plant.time (unknown), line.time (failed) ended: a frame that \
         the node took may not be on disk"
    );
}

/// Maps a refusal of the session to `Refused`, a `ConnectionError`, and a refusal of a
/// stream to `Stopped`, which is not one, each with the code of the refusal and its
/// text.
#[test]
fn error_raises_refused_or_stopped_with_the_code_of_each_refusal() {
    let codes = [
        (program::Refusal::Malformed, "malformed"),
        (program::Refusal::Unknown, "unknown"),
        (program::Refusal::NotHome, "not_home"),
        (program::Refusal::Failed, "failed"),
        (program::Refusal::Busy, "busy"),
        (program::Refusal::Refused, "refused"),
        (program::Refusal::Unsynced, "unsynced"),
        (program::Refusal::Stale, "stale"),
        (program::Refusal::Via, "via"),
        (program::Refusal::Expired, "expired"),
        (program::Refusal::Changed, "changed"),
    ];
    Python::initialize();
    Python::attach(|py| {
        for (refusal, code) in codes {
            let stream = sdk::Error::Stream(program::Error::Refused(refusal));
            let session = sdk::Error::Session(program::Error::Refused(refusal));
            for (variant, class, session) in
                [(stream, "Stopped", false), (session, "Refused", true)]
            {
                let error = super::error(py, &variant);
                assert_eq!(error.get_type(py).name().unwrap().to_string(), class);
                let connection = error.is_instance_of::<PyConnectionError>(py);
                assert_eq!(connection, session, "{variant}");
                let value = error.value(py);
                assert_eq!(value.to_string(), refusal.to_string());
                let given: String = value.getattr("code").unwrap().extract().unwrap();
                assert_eq!(given, code);
            }
        }
    });
}

#[test]
fn a_pin_keeps_the_key_that_another_program_pinned_after_the_read() {
    let dir = env::temp_dir().join(format!("foundation-pin-{}", std::process::id()));
    let path = dir.join("profile.hcl");
    let at: SocketAddr = "10.0.0.5:7400".parse().unwrap();
    let [first, second] = [1, 2].map(|byte| PrivateKey([byte; 32]).public());
    Python::initialize();
    Python::attach(|_| {
        let profile = Profile {
            path: Some(path.clone()),
        };
        fs::create_dir_all(&dir).unwrap();
        let text = format!(
            "node \"{at}\" {{\n  public_key = \"{}\"\n}}\n",
            config::openssh::line(first)
        );
        fs::write(&path, &text).unwrap();
        assert_eq!(profile.pin(at, second).unwrap(), first);
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    });
    fs::remove_dir_all(&dir).unwrap();
}

/// Pins in `<path>.lock` only after a rival program lets go of it, and keeps the key
/// that the rival pinned. A pin that takes no lock fails it only when the pin reads
/// the file in the 200 ms before the rival writes it.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the rival opens the lock, and the pin runs on another thread"
)]
fn a_pin_waits_for_the_lock_of_a_rival() {
    let dir = env::temp_dir().join(format!("foundation-lock-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("profile.hcl");
    let at: SocketAddr = "10.0.0.5:7400".parse().unwrap();
    let [first, second] = [1, 2].map(|byte| PrivateKey([byte; 32]).public());
    let rival = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("profile.hcl.lock"))
        .unwrap();
    rival.lock().unwrap();
    let profile = Profile {
        path: Some(path.clone()),
    };
    let pinning = thread::spawn(move || profile.pin(at, second).map_err(drop));
    thread::sleep(Duration::from_millis(200));
    assert!(!pinning.is_finished());
    let text = format!(
        "node \"{at}\" {{\n  public_key = \"{}\"\n}}\n",
        config::openssh::line(first)
    );
    fs::write(&path, &text).unwrap();
    rival.unlock().unwrap();
    assert_eq!(pinning.join().unwrap(), Ok(first));
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
    fs::remove_dir_all(&dir).unwrap();
}

/// Writes a profile that a bare file name names in the working directory.
#[test]
fn a_pin_writes_a_profile_of_a_bare_name_in_the_working_directory() {
    let name = format!("foundation-bare-{}.hcl", std::process::id());
    let at: SocketAddr = "10.0.0.5:7400".parse().unwrap();
    let key = PrivateKey([4; 32]).public();
    let profile = Profile {
        path: Some(PathBuf::from(&name)),
    };
    let pinned = profile.pin(at, key).map_err(drop);
    let text = fs::read_to_string(&name);
    fs::remove_file(&name).unwrap();
    fs::remove_file(format!("{name}.lock")).unwrap();
    assert_eq!(pinned, Ok(key));
    assert_eq!(
        text.unwrap(),
        format!(
            "node \"{at}\" {{\n  public_key = \"{}\"\n}}\n",
            config::openssh::line(key)
        )
    );
}

/// Only Unix: the test takes the read right of the directory with a Unix mode.
/// Raises the error of the sync of the directory of a pin that cannot read its
/// directory: the pin is on the disk only once the entry of its file is.
#[cfg(unix)]
#[test]
fn a_pin_raises_the_error_of_the_sync_of_its_directory() {
    use std::os::unix::fs::PermissionsExt;

    let dir = env::temp_dir().join(format!("foundation-sync-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o300)).unwrap();
    let path = dir.join("profile.hcl");
    let at: SocketAddr = "10.0.0.5:7400".parse().unwrap();
    let key = PrivateKey([5; 32]).public();
    let pinned = Profile {
        path: Some(path.clone()),
    }
    .pin(at, key);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir_all(&dir).unwrap();
    Python::initialize();
    let error = Python::attach(|py| {
        let error = pinned.expect_err("the directory cannot be read");
        assert!(error.is_instance_of::<pyo3::exceptions::PyOSError>(py));
        error.value(py).to_string()
    });
    let denied = format!("{}: Permission denied (os error 13)", path.display());
    assert_eq!(error, denied);
}

/// Only Unix: the test makes a link with `std::os::unix::fs::symlink`.
/// Writes the file that a linked profile names, with mode 0600, and keeps the link.
#[cfg(unix)]
#[test]
fn a_pin_keeps_a_linked_profile_a_link() {
    use std::os::unix::fs::PermissionsExt;

    let dir = env::temp_dir().join(format!("foundation-link-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let target = dir.join("dotfiles.hcl");
    let path = dir.join("profile.hcl");
    fs::write(&target, "strict = false\n").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let at: SocketAddr = "10.0.0.5:7400".parse().unwrap();
    let key = PrivateKey([3; 32]).public();
    let profile = Profile {
        path: Some(path.clone()),
    };
    assert_eq!(profile.pin(at, key).map_err(drop), Ok(key));
    assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        format!(
            "strict = false\n\nnode \"{at}\" {{\n  public_key = \"{}\"\n}}\n",
            config::openssh::line(key)
        )
    );
    let mode = fs::metadata(&target).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let mut left: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    left.sort();
    assert_eq!(left, ["dotfiles.hcl", "dotfiles.hcl.lock", "profile.hcl"]);
    fs::remove_dir_all(&dir).unwrap();
}

/// Only Unix: the test makes a link with `std::os::unix::fs::symlink`.
/// Writes the file that a profile links to when that file is not there yet, and keeps
/// the link.
#[cfg(unix)]
#[test]
fn a_pin_of_a_profile_that_links_to_no_file_keeps_the_link() {
    let dir = env::temp_dir().join(format!("foundation-dangle-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let target = dir.join("dotfiles.hcl");
    let path = dir.join("profile.hcl");
    std::os::unix::fs::symlink(&target, &path).unwrap();
    let at: SocketAddr = "10.0.0.5:7400".parse().unwrap();
    let key = PrivateKey([3; 32]).public();
    let profile = Profile {
        path: Some(path.clone()),
    };
    assert_eq!(profile.pin(at, key).map_err(drop), Ok(key));
    let linked = fs::symlink_metadata(&path).unwrap().is_symlink();
    let written = fs::read_to_string(&target).ok();
    fs::remove_dir_all(&dir).unwrap();
    assert!(linked, "the profile is no longer a link");
    assert_eq!(
        written,
        Some(format!(
            "node \"{at}\" {{\n  public_key = \"{}\"\n}}\n",
            config::openssh::line(key)
        ))
    );
}

/// Only Unix: the user that runs the tests cannot read `/root`, so a path that
/// expands `~root` fails. A `private_key` of `~root/k.key` is in the directory of the
/// profile.
#[cfg(unix)]
#[test]
fn a_private_key_of_a_tilde_and_a_user_is_in_the_directory_of_the_profile() {
    let dir = env::temp_dir().join(format!("foundation-tilde-{}", std::process::id()));
    fs::create_dir_all(dir.join("~root")).unwrap();
    let key = PrivateKey([5; 32]);
    fs::write(
        dir.join("~root/k.key"),
        config::openssh::key_file(&key, "k"),
    )
    .unwrap();
    let profile = Profile {
        path: Some(dir.join("profile.hcl")),
    };
    Python::initialize();
    let read = Python::attach(|py| {
        let subject: types::name::Name = "alice".parse().unwrap();
        profile
            .private_key(py, &subject, Some(Path::new("~root/k.key")))
            .map(|key| key.0)
            .map_err(|e| e.value(py).to_string())
    });
    fs::remove_dir_all(&dir).unwrap();
    assert_eq!(read, Ok(key.0));
}

/// Only Unix: a test makes a link only there. A relative `private_key` of a profile
/// that is a link to a file in another directory is next to the link.
#[cfg(unix)]
#[test]
fn a_private_key_of_a_linked_profile_is_next_to_the_link() {
    let dir = env::temp_dir().join(format!("foundation-linked-{}", std::process::id()));
    for within in ["links", "real"] {
        fs::create_dir_all(dir.join(within)).unwrap();
    }
    fs::write(dir.join("real/profile.hcl"), "").unwrap();
    std::os::unix::fs::symlink(
        dir.join("real/profile.hcl"),
        dir.join("links/profile.hcl"),
    )
    .unwrap();
    let (mine, theirs) = (PrivateKey([5; 32]), PrivateKey([6; 32]));
    fs::write(
        dir.join("links/mine.key"),
        config::openssh::key_file(&mine, "k"),
    )
    .unwrap();
    fs::write(
        dir.join("real/mine.key"),
        config::openssh::key_file(&theirs, "k"),
    )
    .unwrap();
    let profile = Profile {
        path: Some(dir.join("links/profile.hcl")),
    };
    Python::initialize();
    let read = Python::attach(|py| {
        let subject: types::name::Name = "alice".parse().unwrap();
        profile
            .private_key(py, &subject, Some(Path::new("mine.key")))
            .map(|key| key.0)
            .map_err(|e| e.value(py).to_string())
    });
    fs::remove_dir_all(&dir).unwrap();
    assert_eq!(read, Ok(mine.0));
}

/// Only Unix: the test makes a link with `std::os::unix::fs::symlink`.
/// Raises `OSError` for a profile that is a link to itself, and takes no lock.
#[cfg(unix)]
#[test]
fn a_pin_of_a_profile_that_links_to_itself_raises() {
    let dir = env::temp_dir().join(format!("foundation-loop-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("profile.hcl");
    std::os::unix::fs::symlink(&path, &path).unwrap();
    let looped = fs::canonicalize(&path).unwrap_err();
    let profile = Profile {
        path: Some(path.clone()),
    };
    let at: SocketAddr = "10.0.0.5:7400".parse().unwrap();
    Python::initialize();
    Python::attach(|py| {
        let error = profile.pin(at, PrivateKey([3; 32]).public()).unwrap_err();
        assert_eq!(error.get_type(py).name().unwrap().to_string(), "OSError");
        let text = format!("{}: {looped}", path.display());
        assert_eq!(error.value(py).to_string(), text);
    });
    let left: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(left, ["profile.hcl"]);
    fs::remove_dir_all(&dir).unwrap();
}
