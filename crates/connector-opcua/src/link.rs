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
        crate::child::run(name, None);
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
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name])
        .env("CC", "/missing/gcc")
        .output()
        .unwrap();
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

/// Each symbol outside the copy and `shim.c` that they may name, in glibc and in the
/// outline atomics of libgcc: on x86-64 with GCC or Clang at each optimization level,
/// and on 64-bit Arm with Clang at `-O2` and `-moutline-atomics`. None gives or takes a
/// heap block, so no block crosses between the allocator of libc and `src/alloc.rs`.
const OUTSIDE: [&str; 44] = [
    "_GLOBAL_OFFSET_TABLE_",
    "__aarch64_cas8_acq_rel",
    "__aarch64_swp8_acq_rel",
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
    "pthread_mutex_destroy",
    "pthread_mutex_init",
    "pthread_mutex_lock",
    "pthread_mutex_unlock",
    "pthread_mutexattr_destroy",
    "pthread_mutexattr_init",
    "pthread_mutexattr_settype",
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

/// The symbols that `nm` with `flag` gives for the archives of this build.
fn symbols(flag: &str) -> std::collections::BTreeSet<String> {
    let out = std::path::Path::new(env!("OUT_DIR"));
    names(&[out.join("libopen62541.a"), out.join("libshim.a")], flag)
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "needs GNU nm and the glibc symbols"
)]
fn the_c_names_only_the_listed_symbols_outside_it() {
    let defined = symbols("--defined-only");
    let undefined = symbols("--undefined-only");
    let outside: Vec<&str> =
        undefined.difference(&defined).map(String::as_str).collect();
    assert!(outside.contains(&"connector_opcua_malloc"), "{outside:?}");
    let unlisted: Vec<&str> = outside
        .into_iter()
        .filter(|name| !OUTSIDE.contains(name))
        .collect();
    assert!(unlisted.is_empty(), "the C names {unlisted:?}");
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
