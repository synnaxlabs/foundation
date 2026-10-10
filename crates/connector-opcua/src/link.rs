//! Checks that the open62541 copy and `shim.c` compile and link.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use env::rng::Rng;

use crate::event::Loop;
use crate::ffi::Bytes;
use crate::ffi::Status;
use crate::ffi::test as ffi;

#[test]
fn the_copy_names_a_status_code() {
    assert_eq!(Status(0).name(), "Good");
    assert_eq!(Status(0x8034_0000).name(), "BadNodeIdUnknown");
    assert_eq!(format!("{:?}", Status(0x8034_0000)), "BadNodeIdUnknown");
}

#[test]
fn the_global_clocks_give_a_fixed_time() {
    // SAFETY: each takes no argument and reads no state.
    let now = unsafe { ffi::UA_DateTime_now() };
    // SAFETY: as above.
    let monotonic = unsafe { ffi::UA_DateTime_nowMonotonic() };
    // SAFETY: as above.
    let offset = unsafe { ffi::UA_DateTime_localTimeUtcOffset() };
    assert_eq!((now, monotonic, offset), (0, 0, 0));
}

/// The constructors and the members of the event loop that abort.
const REFUSED: [&str; 9] = [
    "UA_EventLoop_new_POSIX",
    "UA_ConnectionManager_new_POSIX_TCP",
    "UA_ConnectionManager_new_POSIX_UDP",
    "UA_ConnectionManager_new_POSIX_Ethernet",
    "UA_InterruptManager_new_POSIX",
    "UA_EventLoop.stop",
    "UA_EventLoop.free",
    "UA_EventLoop.registerEventSource",
    "UA_EventLoop.deregisterEventSource",
];

/// The variable that names the call that `call_refused` makes.
const CHILD: &str = "CONNECTOR_OPCUA_REFUSED";

/// Makes the call that `CHILD` names, and does nothing when it is not set.
/// `each_refused_call_prints_its_name_and_aborts` sets it in a child process.
#[test]
fn call_refused() {
    #[expect(
        clippy::disallowed_methods,
        reason = "the parent test picks the call that its child process makes"
    )]
    let Ok(name) = std::env::var(CHILD) else {
        return;
    };
    let empty = || Bytes {
        length: 0,
        data: std::ptr::null_mut(),
    };
    match name.as_str() {
        // SAFETY: it aborts before it reads its argument.
        "UA_EventLoop_new_POSIX" => unsafe {
            ffi::UA_EventLoop_new_POSIX(std::ptr::null())
        },
        // SAFETY: as above.
        "UA_ConnectionManager_new_POSIX_TCP" => unsafe {
            ffi::UA_ConnectionManager_new_POSIX_TCP(empty())
        },
        // SAFETY: as above.
        "UA_ConnectionManager_new_POSIX_UDP" => unsafe {
            ffi::UA_ConnectionManager_new_POSIX_UDP(empty())
        },
        // SAFETY: as above.
        "UA_ConnectionManager_new_POSIX_Ethernet" => unsafe {
            ffi::UA_ConnectionManager_new_POSIX_Ethernet(empty())
        },
        // SAFETY: as above.
        "UA_InterruptManager_new_POSIX" => unsafe {
            ffi::UA_InterruptManager_new_POSIX(empty())
        },
        _ => call_member(&name),
    };
}

/// Calls the member of the event loop that `name` names.
fn call_member(name: &str) -> ! {
    let mut sim = sim::Sim::new(sim::Config::default());
    let clock = sim.node(sim::node::Config::default()).clock();
    let events = Loop::new(clock, &mut Rng::from_seed(0));
    let (members, raw, none) = (events.members(), events.raw(), std::ptr::null_mut());
    let status = match name {
        "UA_EventLoop.stop" => {
            // SAFETY: each member takes its own loop, and aborts before it reads more.
            unsafe { (members.stop)(raw) };
            Status::GOOD
        }
        // SAFETY: as above.
        "UA_EventLoop.free" => Status(unsafe { (members.free)(raw) }),
        "UA_EventLoop.registerEventSource" => {
            // SAFETY: as above.
            Status(unsafe { (members.register)(raw, none) })
        }
        "UA_EventLoop.deregisterEventSource" => {
            // SAFETY: as above.
            Status(unsafe { (members.deregister)(raw, none) })
        }
        _ => panic!("no refused call is named {name}"),
    };
    panic!("{name} gave {status:?}")
}

#[test]
#[cfg(unix)]
fn each_refused_call_prints_its_name_and_aborts() {
    use std::os::unix::process::ExitStatusExt;
    const SIGABRT: i32 = 6;
    for name in REFUSED {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "link::call_refused"])
            .env(CHILD, name)
            .output()
            .unwrap();
        assert_eq!(output.status.signal(), Some(SIGABRT), "{name}");
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            format!("connector-opcua: open62541 called {name}, which is not built\n")
        );
    }
}

/// The directory of this crate.
const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// The compiler of the build of the shim, at `-O3`, the release level, where GCC gives
/// the most warnings. A warning is an error in the shim.
fn shim_compiler() -> cc::Tool {
    let copy = std::path::Path::new(ROOT).join("../../patches/open62541");
    let flags = std::fs::read_to_string(copy.join("flags.txt")).unwrap();
    let target = env!("CONNECTOR_OPCUA_TARGET");
    let mut shim = crate::compiler::builds(&copy, &flags, "").shim;
    shim.target(target)
        .host(target)
        .opt_level(3)
        .cargo_metadata(false)
        .cargo_warnings(false)
        .get_compiler()
}

/// Gives each pragma of `shim.c` after the preprocessor, which joins split lines and
/// expands `_Pragma`.
fn shim_pragmas() -> Vec<String> {
    let path = std::path::Path::new(ROOT).join("src/shim.c");
    let output = shim_compiler()
        .to_command()
        .arg("-E")
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let mut in_shim = false;
    let mut pragmas = Vec::new();
    for line in String::from_utf8(output.stdout).unwrap().lines() {
        // A line marker, `# <line> "<file>" <flags>`, names the file of the next lines.
        if let Some(marker) = line.strip_prefix("# ") {
            in_shim = marker.contains(&format!("\"{}\"", path.display()));
        } else if in_shim && line.trim_start().starts_with("#pragma") {
            pragmas.push(line.trim().to_owned());
        }
    }
    pragmas
}

/// Compiles `shim.c` with `line` added after the line `after`, with `shim_compiler`.
/// Gives the compiler's errors, which are empty when it compiles.
fn check_shim(after: &str, line: &str) -> String {
    use std::io::Write;
    use std::process::Stdio;

    let tool = shim_compiler();
    let text =
        std::fs::read_to_string(std::path::Path::new(ROOT).join("src/shim.c")).unwrap();
    let (at, _) = text
        .match_indices(after)
        .next()
        .expect("shim.c holds the line");
    let at = at + after.len();
    let source = format!("{}\n{line}{}", &text[..at], &text[at..]);
    let mut child = tool
        .to_command()
        .args(["-c", "-o", "/dev/null", "-x", "c", "-"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let errors = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.success(), errors.is_empty(), "{errors}");
    errors
}

#[test]
fn the_shim_ignores_only_the_unused_parameters_of_the_headers() {
    // `cc` reads C flags from the environment, and `-w` there hides each warning.
    if !crate::child::running() {
        let name = "link::the_shim_ignores_only_the_unused_parameters_of_the_headers";
        crate::child::run(name, &[]);
        return;
    }
    assert_eq!(
        shim_pragmas(),
        [
            "#pragma GCC diagnostic push",
            "#pragma GCC diagnostic ignored \"-Wunused-parameter\"",
            "#pragma GCC diagnostic pop",
        ]
    );
    let headers = "#include <open62541/types.h>";
    assert_eq!(check_shim(headers, ""), "");
    let parameter = "int in_the_headers(int unused) { return 0; }";
    assert_eq!(check_shim(headers, parameter), "");
    let variable = check_shim(
        headers,
        "int in_the_headers(void) { int unused; return 0; }",
    );
    assert!(variable.contains("unused variable 'unused'"), "{variable}");
    // GCC gives this one only when it compiles, not with `-fsyntax-only`.
    let function = check_shim(headers, "static int in_the_headers(void) { return 0; }");
    assert!(function.contains("unused-function"), "{function}");
    // GCC gives this one only when it optimizes.
    let bounds = "int in_the_headers(void) { int a[2] = {0, 0}; return a[3]; }";
    let bounds = check_shim(headers, bounds);
    assert!(bounds.contains("array-bounds"), "{bounds}");
    let body = check_shim(
        "#include <stdlib.h>",
        "int in_the_body(int unused) { return 0; }",
    );
    assert!(body.contains("unused parameter 'unused'"), "{body}");
}

#[test]
fn the_shim_check_ignores_the_environment_of_cc() {
    let name = "link::the_shim_ignores_only_the_unused_parameters_of_the_headers";
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name])
        .env("CC", "cc -w")
        .env("CFLAGS", "-w")
        .env("CRATE_CC_NO_DEFAULTS", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed"),
        "{stdout}"
    );
}

#[test]
fn the_shim_check_uses_the_compiler_of_cc() {
    let name = "link::the_shim_ignores_only_the_unused_parameters_of_the_headers";
    let output = crate::child::output(name, &[("CC", "/missing/gcc")]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("No such file or directory"), "{stdout}");
}

unsafe extern "C" {
    fn UA_Timer_init(timer: *mut std::ffi::c_void);
    fn UA_Timer_next(timer: *mut std::ffi::c_void) -> i64;
    fn UA_Timer_process(timer: *mut std::ffi::c_void, now: i64) -> i64;
    fn UA_Timer_remove(timer: *mut std::ffi::c_void, key: u64);
    fn UA_Timer_clear(timer: *mut std::ffi::c_void);
}

/// The library does not compile `timer.c`, and the archive drops an object that
/// nothing names. So this test links only when `sources.txt` holds it.
#[test]
fn the_copy_links_the_timer() {
    let functions = std::hint::black_box([
        UA_Timer_init as *const (),
        UA_Timer_next as *const (),
        UA_Timer_process as *const (),
        UA_Timer_remove as *const (),
        UA_Timer_clear as *const (),
    ]);
    let distinct: std::collections::BTreeSet<_> = functions.iter().collect();
    assert_eq!(distinct.len(), functions.len());
}

/// Each symbol outside the copy and `shim.c` that they may name in glibc: on x86-64
/// with GCC or Clang at each optimization level, and on 64-bit Arm with Clang at
/// `-O2` and `-moutline-atomics`. None gives or takes a heap block, so no block
/// crosses between the allocator of libc and `src/alloc.rs`.
const OUTSIDE: [&str; 35] = [
    "_GLOBAL_OFFSET_TABLE_",
    "__ctype_b_loc",
    "__errno_location",
    "__fprintf_chk",
    "__memcpy_chk",
    "__memmove_chk",
    "__memset_chk",
    "__printf_chk",
    "__stack_chk_fail",
    "__stack_chk_guard",
    "__syslog_chk",
    "__tls_get_addr",
    "abort",
    "access",
    "bcmp",
    "connector_opcua_calloc",
    "connector_opcua_free",
    "connector_opcua_malloc",
    "connector_opcua_realloc",
    "fflush",
    "fprintf",
    "memcmp",
    "memcpy",
    "memmove",
    "memset",
    "printf",
    "puts",
    "stderr",
    "stdout",
    "strcmp",
    "strlen",
    "strncmp",
    "strtod",
    "syslog",
    "write",
];

/// At `UA_MULTITHREADING` 0 the copy takes no lock and calls no atomic, so the symbol
/// tests fail on each such name only while the list holds none.
#[test]
fn the_list_holds_no_lock_and_no_atomic() {
    let held: Vec<_> = OUTSIDE
        .iter()
        .filter(|name| name.starts_with("pthread_") || name.starts_with("__aarch64_"))
        .collect();
    assert!(held.is_empty(), "{held:?}");
}

/// Whether `name` is in the runtime of the address or the undefined behavior
/// sanitizer, which `build.rs` adds together.
fn sanitizer(name: &str) -> bool {
    name.starts_with("__asan_")
        || name.starts_with("__ubsan_")
        || ["__start_asan_globals", "__stop_asan_globals"].contains(&name)
}

#[test]
fn sanitizer_names_only_the_runtimes() {
    for name in ["__asan_init", "__ubsan_handle_add_overflow_abort"] {
        assert!(sanitizer(name), "{name}");
    }
    assert!(sanitizer("__start_asan_globals"));
    assert!(sanitizer("__stop_asan_globals"));
    for name in [
        "memcpy",
        "__start_other",
        "asan_init",
        "__msan_init",
        "_asan_x",
    ] {
        assert!(!sanitizer(name), "{name}");
    }
}

/// The symbols that `nm` with `flag` gives for `files`.
fn names(
    files: &[std::path::PathBuf],
    flag: &str,
) -> std::collections::BTreeSet<String> {
    let output = std::process::Command::new("nm")
        .args(["-P", flag])
        .args(files)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    // A line that names a file or an object of an archive has one word.
    text.lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let name = words.next()?;
            words.next().map(|_| name.to_owned())
        })
        .collect()
}

/// The symbols that `nm` with `flag` gives for the archive of this build, which holds
/// the copy and the shim.
fn symbols(flag: &str) -> std::collections::BTreeSet<String> {
    let out = std::path::Path::new(env!("OUT_DIR"));
    names(&[out.join("libopen62541.a")], flag)
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "needs GNU nm and the glibc symbols"
)]
fn the_c_names_only_the_listed_symbols_outside_it() {
    let defined = symbols("--defined-only");
    assert!(
        defined.contains("shim_client_new"),
        "the archive holds no shim"
    );
    let undefined = symbols("--undefined-only");
    let outside: Vec<&str> =
        undefined.difference(&defined).map(String::as_str).collect();
    assert!(outside.contains(&"connector_opcua_malloc"), "{outside:?}");
    let unlisted: Vec<&str> = outside
        .into_iter()
        .filter(|name| !(OUTSIDE.contains(name) || (cfg!(asan) && sanitizer(name))))
        .collect();
    assert!(unlisted.is_empty(), "the C names {unlisted:?}");
}

/// The shim takes no value from the PCG32 generator of the copy, which OPEN62541
/// SOURCE bars for each nonce, key, and session token of Foundation code.
#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs GNU nm")]
fn the_shim_draws_nothing_from_the_generator_of_the_copy() {
    let shim: Vec<_> = std::fs::read_dir(env!("OUT_DIR"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.to_string_lossy().ends_with("-shim.o"))
        .collect();
    assert_eq!(shim.len(), 1, "{shim:?}");
    let undefined = names(&shim, "--undefined-only");
    assert!(
        undefined.contains("UA_Client_newWithConfig"),
        "{undefined:?}"
    );
    let drawn: Vec<_> = ["UA_UInt32_random", "UA_Guid_random"]
        .into_iter()
        .filter(|name| undefined.contains(*name))
        .collect();
    assert!(drawn.is_empty(), "the shim calls {drawn:?}");
}

/// The directory of the crate.
fn root() -> std::path::PathBuf {
    std::path::Path::new(ROOT).canonicalize().unwrap()
}

/// The `src/` directory of the crate.
fn src() -> std::path::PathBuf {
    root().join("src")
}

/// Each identifier for which `named` holds, as `path: identifier`, in the Rust
/// outside tests of the crate at `root`: each `.rs` file outside `tests/` and each
/// file that a build of the library reads, less each file that a test build reads and
/// no build of the library reads. The builds are those on this host with each set of
/// the features of `Cargo.toml`, with and without debug assertions. Comments and inline
/// modules count.
fn named_outside_tests(
    root: &std::path::Path,
    named: impl Fn(&str) -> bool,
) -> Vec<String> {
    let features = features(root);
    let on = |set: usize| -> Vec<String> {
        let features = features.iter().enumerate();
        features
            .filter(|(bit, _)| set >> bit & 1 == 1)
            .flat_map(|(_, feature)| {
                ["--cfg".to_owned(), format!("feature=\"{feature}\"")]
            })
            .collect()
    };
    let mut built = Vec::new();
    for set in 0..1 << features.len() {
        for assertions in ["on", "off"] {
            let flag = format!("-Cdebug-assertions={assertions}");
            built.extend(read_by_rustc(root, &[on(set), vec![flag]].concat()));
        }
    }
    let every = on((1 << features.len()) - 1);
    let tested = read_by_rustc(root, &[every, vec!["--test".to_owned()]].concat());
    let tests = root.join("tests");
    let mut files = files_under(root, "rs");
    files.retain(|path| !path.starts_with(&tests));
    files.extend(built.iter().cloned());
    files.sort();
    files.dedup();
    files
        .into_iter()
        .filter(|path| built.contains(path) || !tested.contains(path))
        .flat_map(|path| named_in(&path, &named))
        .collect()
}

/// The features of the `Cargo.toml` of `root`.
///
/// # Panics
///
/// On a line of its `[features]` table that is not blank, a comment, or
/// `<name> = [...]`, so that no feature is left out.
fn features(root: &std::path::Path) -> Vec<String> {
    let manifest = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
    let table = manifest
        .lines()
        .skip_while(|line| *line != "[features]")
        .skip(1)
        .take_while(|line| !line.starts_with('['));
    table
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let feature = line.split_once(" = [").filter(|(name, rest)| {
                let named = |c: char| c.is_ascii_alphanumeric() || "_-".contains(c);
                rest.ends_with(']') && name.chars().all(named)
            });
            let (name, _) = feature.unwrap_or_else(|| {
                panic!("the scan cannot read the line `{line}` of [features]")
            });
            name.to_owned()
        })
        .collect()
}

/// Each file that rustc reads to expand the library of the crate at `root`, at
/// `src/lib.rs`, with `flags`. No other crate is given, so its names fail to resolve,
/// but rustc lists the files of the crate still.
fn read_by_rustc(root: &std::path::Path, flags: &[String]) -> Vec<std::path::PathBuf> {
    let output = std::process::Command::new("rustc")
        .args([
            "--edition",
            "2024",
            "--crate-type",
            "lib",
            "--emit",
            "dep-info=-",
        ])
        .args(flags)
        .arg(root.join("src/lib.rs"))
        .output()
        .unwrap();
    let deps = String::from_utf8(output.stdout).unwrap();
    assert!(
        !deps.is_empty(),
        "rustc gives no files: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    deps.lines()
        .filter_map(|line| line.strip_suffix(':'))
        .map(|path| std::fs::canonicalize(path.replace("\\ ", " ")).unwrap())
        .collect()
}

/// Each file under `dir` with the extension `extension`, sorted.
fn files_under(dir: &std::path::Path, extension: &str) -> Vec<std::path::PathBuf> {
    let mut dirs = vec![dir.to_path_buf()];
    let mut files = Vec::new();
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|found| found == extension) {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// The identifiers and numbers of `text`.
fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
}

/// Each identifier of `path` for which `named` holds, as `path: identifier`. A byte
/// that is not UTF-8 splits identifiers.
#[expect(
    clippy::disallowed_methods,
    reason = "a test reads the files of the crate"
)]
fn named_in(path: &std::path::Path, named: impl Fn(&str) -> bool) -> Vec<String> {
    let text = String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned();
    let found = words(&text).filter(|word| named(word));
    found
        .map(|word| format!("{}: {word}", path.display()))
        .collect()
}

/// The scan names `shim_client_new` in each file outside tests that names it, and in
/// no test file.
#[test]
fn the_scan_names_only_the_files_outside_tests() {
    let src = src();
    let named = named_outside_tests(&root(), |name| name == "shim_client_new");
    let at = |file: &str| format!("{}: shim_client_new", src.join(file).display());
    assert_eq!(named, [at("bench.rs"), at("ffi.rs")]);
}

/// A file that only a test build reads is cut off, with the files that it declares.
#[test]
fn the_scan_cuts_off_only_the_files_of_a_test_build() {
    let root = create_tree(
        "gated",
        &[
            MANIFEST,
            (
                "src/lib.rs",
                "#[cfg(test)]\n#[allow(unused)]\nmod t;\nmod p;\n",
            ),
            ("src/t.rs", "mod u;"),
            ("src/t/u.rs", "fn named() {}"),
            ("src/p.rs", "fn named() {}"),
        ],
    );
    let named = named_outside_tests(&root, |name| name == "named");
    assert_eq!(named, [at(&root, "src/p.rs")]);
}

/// A file that a build of the library reads is not cut off when a test build reads it
/// too: with a feature off, or with no debug assertions.
#[test]
fn the_scan_cuts_off_no_file_of_a_build_of_the_library() {
    let root = create_tree(
        "built",
        &[
            MANIFEST,
            (
                "src/lib.rs",
                "#[cfg(any(test, not(feature = \"sim\")))]\nmod f;\n\
                 #[cfg(any(test, not(debug_assertions)))]\nmod d;\n",
            ),
            ("src/f.rs", "fn named() {}"),
            ("src/d.rs", "fn named() {}"),
        ],
    );
    let named = named_outside_tests(&root, |name| name == "named");
    assert_eq!(named, [at(&root, "src/d.rs"), at(&root, "src/f.rs")]);
}

/// The scan reads a file outside `src/` that a `path` attribute, also under
/// `cfg_attr`, or an `include!` gives to the crate, and cuts off the file that the
/// test build reads in its place.
#[test]
fn the_scan_reads_each_file_that_the_crate_includes() {
    let root = create_tree(
        "path",
        &[
            MANIFEST,
            (
                "src/lib.rs",
                "#[path = \"../p.rs\"]\nmod p;\n\
                 #[cfg_attr(not(test), path = \"../q.rs\")]\nmod q;\n\
                 include!(\"../i.rs\");\n",
            ),
            ("p.rs", "fn named() {}"),
            ("q.rs", "fn named() {}"),
            ("i.rs", "fn named() {}"),
            ("src/q.rs", "fn named() {}"),
        ],
    );
    let named = named_outside_tests(&root, |name| name == "named");
    assert_eq!(
        named,
        [at(&root, "i.rs"), at(&root, "p.rs"), at(&root, "q.rs")]
    );
}

/// The scan reads each `.rs` file of the crate outside `tests/`, also one that no
/// build on this host reads: under a feature that is off, another target, a macro of
/// another crate, or no build at all.
#[test]
fn the_scan_reads_each_file_that_a_build_on_another_host_may_read() {
    let lib = format!(
        "#[cfg(not(feature = \"sim\"))]\n#[path = \"../off.rs\"]\nmod off;\n\
         #[cfg_attr(not(feature = \"sim\"), path = \"../real.rs\")]\nmod x;\n\
         #[cfg(not(target_os = \"{}\"))]\n#[path = \"../other.rs\"]\nmod other;\n\
         cfg_if::cfg_if! {{ if #[cfg(unix)] {{ #[path = \"../u.rs\"] mod u; }} }}\n\
         #[cfg(any())]\nmod n;\n",
        std::env::consts::OS
    );
    let root = create_tree(
        "other",
        &[
            MANIFEST,
            ("src/lib.rs", &lib),
            ("src/x.rs", ""),
            ("off.rs", "fn named() {}"),
            ("real.rs", "fn named() {}"),
            ("other.rs", "fn named() {}"),
            ("u.rs", "fn named() {}"),
            ("src/n.rs", "fn named() {}"),
            ("tests/t.rs", "fn named() {}"),
        ],
    );
    let named = named_outside_tests(&root, |name| name == "named");
    let files = ["off.rs", "other.rs", "real.rs", "src/n.rs", "u.rs"];
    assert_eq!(named, files.map(|file| at(&root, file)));
}

/// The scan reads a file that the crate includes as bytes that are not UTF-8.
#[test]
fn the_scan_reads_a_file_that_is_not_utf_8() {
    let root = create_tree(
        "bytes",
        &[
            MANIFEST,
            (
                "src/lib.rs",
                "pub static B: &[u8] = include_bytes!(\"../b.der\");\n",
            ),
        ],
    );
    std::fs::write(root.join("b.der"), b"\xffnamed\xfe").unwrap();
    let named = named_outside_tests(&root, |name| name == "named");
    assert_eq!(named, [at(&root, "b.der")]);
}

/// The scan refuses a line of `[features]` that it cannot read, so it leaves out no
/// feature: each form of TOML other than `<name> = [...]` on one line.
#[test]
fn the_scan_refuses_a_line_of_features_that_it_cannot_read() {
    for (line, table) in [
        ("sim=[]", "sim=[]\n"),
        ("\"sim\" = []", "\"sim\" = []\n"),
        ("sim = [", "sim = [\n  \"a\",\n]\n"),
    ] {
        let manifest = format!("[features]\n{table}");
        let root =
            create_tree("features", &[("Cargo.toml", &manifest), ("src/lib.rs", "")]);
        let refused =
            std::panic::catch_unwind(|| named_outside_tests(&root, |_| false));
        let message = *refused.unwrap_err().downcast::<String>().unwrap();
        let expected = format!("the scan cannot read the line `{line}` of [features]");
        assert_eq!(message, expected);
    }
}

/// A test mock that a `path` attribute gives in place of a module is cut off, and
/// the module is read.
#[test]
fn the_scan_reads_the_module_that_a_test_mock_shadows() {
    let root = create_tree(
        "mock",
        &[
            MANIFEST,
            (
                "src/lib.rs",
                "#[cfg(test)]\n#[path = \"mock.rs\"]\nmod clock;\n\
                 #[cfg(not(test))]\nmod clock;\n",
            ),
            ("src/mock.rs", "fn named() {}"),
            ("src/clock.rs", "fn named() {}"),
        ],
    );
    let named = named_outside_tests(&root, |name| name == "named");
    assert_eq!(named, [at(&root, "src/clock.rs")]);
}

/// The `Cargo.toml` of a tree, with one feature.
const MANIFEST: (&str, &str) = ("Cargo.toml", "[features]\nsim = []\n");

/// How [`named_outside_tests`] gives the name `named` in `file` of `root`.
fn at(root: &std::path::Path, file: &str) -> String {
    format!("{}: named", root.join(file).display())
}

/// A directory under `OUT_DIR` named `name` that holds only `files`.
fn create_tree(name: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
    let dir = std::path::Path::new(env!("OUT_DIR"))
        .join("scan")
        .join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).unwrap();
    }
    for (path, text) in files {
        let path = dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    dir.canonicalize().unwrap()
}

/// Outside tests, the Rust of the crate names neither PCG32 draw of the copy, so it
/// cannot bind or call one.
#[test]
fn the_rust_draws_nothing_from_the_generator_of_the_copy() {
    let drawn = named_outside_tests(&root(), |name| {
        ["UA_UInt32_random", "UA_Guid_random"].contains(&name)
    });
    assert!(drawn.is_empty(), "the Rust names {drawn:?}");
}

/// Outside tests, the Rust builds no server and names no `UA_random_seed`, so it
/// reaches none of `setDefaultConfig`, `interruptServer`, and `UA_random_seed`, which
/// `cargo xtask open62541` lets call the global clock.
#[test]
fn the_rust_outside_tests_builds_no_server() {
    let named = named_outside_tests(&root(), |name| {
        name.starts_with("UA_Server")
            || ["shim_server_new", "UA_random_seed"].contains(&name)
    });
    assert!(named.is_empty(), "the Rust outside tests names {named:?}");
}

/// `UA_Client_new` gives a client the stdout logger, whose `UA_Log_Stdout_log`
/// `cargo xtask open62541` lets call the global clock. Outside tests, the Rust names
/// neither it nor a `UA_Log_Stdout` function. `shim_client_new` gives a client the
/// logger of its loop, and the stderr tests of `event` check that.
#[test]
fn the_rust_outside_tests_gives_no_client_the_stdout_logger() {
    let named = named_outside_tests(&root(), |name| {
        name.starts_with("UA_Client_new") || name.starts_with("UA_Log_Stdout")
    });
    assert!(named.is_empty(), "the Rust outside tests names {named:?}");
}

/// `cargo xtask open62541` lets `interruptServer` and `UA_random_seed` call the global
/// clock. The C builds with `-ffunction-sections`, and Rust links with
/// `--gc-sections`, so this test binary keeps only the functions that the tests reach,
/// and the test server reaches neither.
#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs GNU nm")]
fn the_tests_reach_neither_interrupt_server_nor_random_seed() {
    let kept = names(&[std::env::current_exe().unwrap()], "--defined-only");
    assert!(
        kept.contains("setDefaultConfig"),
        "nm gives no local symbol"
    );
    let reached: Vec<_> = ["interruptServer", "UA_random_seed"]
        .into_iter()
        .filter(|name| kept.contains(*name))
        .collect();
    assert!(reached.is_empty(), "the test binary keeps {reached:?}");
}

/// Checks the reason of `encryptUserIdentityTokenEcc` in `CLOCK_CALLS` of `cargo xtask
/// open62541`, with [`the_rust_and_the_shim_give_a_client_no_security_policy`].
#[test]
fn the_shim_refuses_encryption() {
    let errors =
        check_shim("#pragma GCC diagnostic pop", "#define UA_ENABLE_ENCRYPTION");
    assert!(
        errors.contains("connector-opcua builds open62541 with encryption off"),
        "{errors}"
    );
}

/// Checks the reason of `encryptUserIdentityTokenEcc` in `CLOCK_CALLS` of `cargo xtask
/// open62541`: the default config of a client with encryption off has only the policy
/// `None`, and outside tests, neither the Rust nor the C under `src/` names a security
/// policy.
#[test]
fn the_rust_and_the_shim_give_a_client_no_security_policy() {
    let policy = |name: &str| {
        name.starts_with("UA_SecurityPolicy")
            || ["securityPolicies", "authSecurityPolicies"].contains(&name)
    };
    let mut named = named_outside_tests(&root(), policy);
    for path in [files_under(&src(), "c"), files_under(&src(), "h")].concat() {
        named.extend(named_in(&path, policy));
    }
    assert!(named.is_empty(), "names {named:?}");
}

/// When this test binary links the address sanitizer, the sanitizer instruments the C
/// and `build.rs` sets `cfg(asan)`, so neither the C checks nor the tests of the
/// poisoning in `alloc` can turn off unseen.
#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs GNU nm")]
fn the_c_and_cfg_asan_follow_the_address_sanitizer_of_the_rust() {
    let exe = std::env::current_exe().unwrap();
    let rust = names(&[exe], "--defined-only").contains("__asan_init");
    let undefined = symbols("--undefined-only");
    let c = undefined.iter().any(|name| name.starts_with("__asan_"));
    assert_eq!(
        (c, cfg!(asan)),
        (rust, rust),
        "(the C calls ASan, cfg(asan)) must equal whether the Rust links ASan"
    );
}

/// GCC 10 and later default to `-moutline-atomics` on 64-bit Arm Linux, and so does
/// Clang with libgcc 9.3.1 or later, or with `-rtlib=compiler-rt`. The host build does
/// not show it. So this preprocesses each source of the copy as the host build does,
/// and compiles it for 64-bit Arm with that default. The preprocessing is the host's,
/// so the test finds the names that the code generation for Arm adds, not the names of
/// a branch of the source for Arm only.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "needs GNU nm, Clang, and the glibc symbols"
)]
fn the_c_on_64_bit_arm_names_only_the_listed_symbols_outside_it() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let copy = root.join("../../patches/open62541");
    let out = std::path::Path::new(env!("OUT_DIR")).join("arm");
    std::fs::create_dir_all(&out).unwrap();
    let flags = std::fs::read_to_string(copy.join("flags.txt")).unwrap();
    let sources = std::fs::read_to_string(copy.join("sources.txt")).unwrap();
    let header = format!(
        "-DUA_ARCH_HEADER=\"{}\"",
        root.join("src/alloc.h").display()
    );
    let objects: Vec<_> = sources
        .lines()
        .enumerate()
        .map(|(i, source)| {
            let text = out.join(format!("{i}.i"));
            let object = out.join(format!("{i}.o"));
            let mut preprocess = std::process::Command::new("clang");
            preprocess.arg("-E").arg(&header);
            for flag in flags.lines() {
                match flag.strip_prefix("-I") {
                    Some(dir) => {
                        preprocess.arg(format!("-I{}", copy.join(dir).display()))
                    }
                    None => preprocess.arg(flag),
                };
            }
            let status = preprocess
                .arg(copy.join(source))
                .arg("-o")
                .arg(&text)
                .status()
                .expect("needs Clang");
            assert!(status.success(), "{source}");
            let status = std::process::Command::new("clang")
                .args([
                    "--target=aarch64-linux-gnu",
                    "-moutline-atomics",
                    "-O2",
                    "-w",
                    "-c",
                ])
                .arg(&text)
                .arg("-o")
                .arg(&object)
                .status()
                .unwrap();
            assert!(status.success(), "{source}");
            object
        })
        .collect();
    let defined = symbols("--defined-only");
    let undefined = names(&objects, "--undefined-only");
    assert!(
        undefined.contains("connector_opcua_malloc"),
        "{undefined:?}"
    );
    let unlisted: Vec<String> = undefined
        .into_iter()
        .filter(|name| !defined.contains(name) && !OUTSIDE.contains(&name.as_str()))
        .collect();
    assert!(unlisted.is_empty(), "the C names {unlisted:?}");
}

/// Clang defines no `__FLOAT_WORD_ORDER__`, so `config.h` must find the float order of
/// each target from its other macros, or the copy encodes floats on its slow path with
/// `long double` helpers. A big-endian target must not copy floats as they lie in
/// memory. Each system header is empty, so only the predefined macros of the target
/// decide the float order.
#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs Clang")]
fn the_copy_copies_floats_as_they_lie_in_memory_on_little_endian_targets() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let generated = root.join("../../patches/open62541/src_generated");
    let config = generated.join("open62541/config.h");
    let empty = std::path::Path::new(env!("OUT_DIR")).join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let text = std::fs::read_to_string(&config).unwrap();
    for header in text.lines().filter_map(|line| {
        let line = line.strip_prefix('#')?.trim_start();
        line.strip_prefix("include")?
            .trim()
            .strip_prefix('<')?
            .strip_suffix('>')
    }) {
        let header = empty.join(header);
        std::fs::create_dir_all(header.parent().unwrap()).unwrap();
        std::fs::write(header, "").unwrap();
    }
    let arch = empty.join("arch.h");
    std::fs::write(&arch, "").unwrap();
    for (target, copied) in [
        ("x86_64-linux-gnu", "1"),
        ("aarch64-linux-gnu", "1"),
        ("arm64-apple-macos", "1"),
        ("aarch64_be-linux-gnu", "0"),
    ] {
        let output = std::process::Command::new("clang")
            .arg(format!("--target={target}"))
            .args(["-nostdinc", "-E", "-dM", "-x", "c"])
            .arg(format!("-DUA_ARCH_HEADER=\"{}\"", arch.display()))
            .arg(format!("-I{}", empty.display()))
            .arg(&config)
            .output()
            .expect("needs Clang");
        let errors = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "{target}: {errors}");
        let macros = String::from_utf8(output.stdout).unwrap();
        let overlayable = macros
            .lines()
            .find_map(|line| line.strip_prefix("#define UA_BINARY_OVERLAYABLE_FLOAT "));
        assert_eq!(overlayable, Some(copied), "{target}");
    }
}
