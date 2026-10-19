use std::env;
use std::ffi::CStr;
use std::path::Path;

use pyo3::exceptions::PyConnectionError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use pyo3::wrap_pymodule;
use types::frame;

const SUITE: &CStr = c"
import io, sys, unittest
sys.modules['foundation._native'] = native
sys.path.insert(0, str(package))
sys.path.insert(0, str(top))
stream = io.StringIO()
suite = unittest.TestLoader().loadTestsFromName(module)
result = unittest.TextTestRunner(stream, verbosity=2).run(suite)
passed = result.wasSuccessful() and result.testsRun > len(result.skipped)
report = stream.getvalue()
";

/// The files of `sdk/python/tests`, each with whether it runs only with the feature
/// `sim`.
const FILES: [(&str, bool); 3] = [
    ("test_client.py", false),
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
            "the frame holds no sample, so it was not sent",
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
            "the writer names no channel",
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
