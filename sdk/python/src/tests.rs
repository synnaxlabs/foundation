use std::env;
use std::ffi::CStr;
use std::fs;
use std::net::SocketAddr;
use std::path::Path;
use std::thread;
use std::time::Duration;

use pyo3::exceptions::PyConnectionError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use pyo3::wrap_pymodule;
use types::ed25519::PrivateKey;
use types::frame;

use crate::profile::Profile;

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
const FILES: [(&str, bool); 4] = [
    ("test_stubs.py", false),
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
        let profile = Profile { path: path.clone() };
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
    let profile = Profile { path: path.clone() };
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
    let profile = Profile { path: path.clone() };
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
    let profile = Profile { path: path.clone() };
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
        path: dir.join("profile.hcl"),
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
        path: dir.join("links/profile.hcl"),
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

/// Raises `OSError` for a profile that is a link to itself, and takes no lock.
#[cfg(unix)]
#[test]
fn a_pin_of_a_profile_that_links_to_itself_raises() {
    let dir = env::temp_dir().join(format!("foundation-loop-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("profile.hcl");
    std::os::unix::fs::symlink(&path, &path).unwrap();
    let looped = fs::canonicalize(&path).unwrap_err();
    let profile = Profile { path: path.clone() };
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
