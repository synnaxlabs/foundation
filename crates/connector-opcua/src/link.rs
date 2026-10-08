//! Checks that the open62541 copy and `shim.c` compile and link.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use crate::ffi::{self, Bytes, Status};

#[test]
fn the_copy_names_a_status_code() {
    assert_eq!(Status(0).name(), "Good");
    assert_eq!(Status(0x8034_0000).name(), "BadNodeIdUnknown");
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

/// The constructors that abort.
const REFUSED: [&str; 5] = [
    "UA_EventLoop_new_POSIX",
    "UA_ConnectionManager_new_POSIX_TCP",
    "UA_ConnectionManager_new_POSIX_UDP",
    "UA_ConnectionManager_new_POSIX_Ethernet",
    "UA_InterruptManager_new_POSIX",
];

/// The variable that names the constructor `call_refused` calls.
const CHILD: &str = "CONNECTOR_OPCUA_REFUSED";

/// Calls the constructor that `CHILD` names, and does nothing when it is not set.
/// `each_posix_constructor_prints_its_name_and_aborts` sets it in a child process.
#[test]
fn call_refused() {
    #[expect(
        clippy::disallowed_methods,
        reason = "the parent test picks the constructor that its child process calls"
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
        _ => panic!("no POSIX constructor is named {name}"),
    };
}

#[test]
#[cfg(unix)]
fn each_posix_constructor_prints_its_name_and_aborts() {
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

/// Compiles `shim.c` with `line` added after the line `after`, with the build of the
/// shim at `-O3`, the release level, where GCC gives the most warnings. Gives the
/// compiler's errors, which are empty when it compiles. A warning is an error in the
/// shim.
fn check_shim(after: &str, line: &str) -> String {
    use std::io::Write;
    use std::path::Path;
    use std::process::Stdio;

    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let copy = root.join("../../patches/open62541");
    let flags = std::fs::read_to_string(copy.join("flags.txt")).unwrap();
    let target = env!("CONNECTOR_OPCUA_TARGET");
    let mut shim = crate::compiler::builds(&copy, &flags, "").shim;
    let tool = shim
        .target(target)
        .host(target)
        .opt_level(3)
        .cargo_metadata(false)
        .cargo_warnings(false)
        .get_compiler();
    let text = std::fs::read_to_string(root.join("src/shim.c")).unwrap();
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
        crate::child::run(name, env!("CONNECTOR_OPCUA_TARGET"), None);
        return;
    }
    let text = include_str!("shim.c");
    let lines = text.lines().map(str::trim);
    let pragmas: Vec<_> = lines
        .filter(|l| l.to_lowercase().contains("pragma"))
        .collect();
    assert_eq!(
        pragmas,
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
    let target = env!("CONNECTOR_OPCUA_TARGET");
    let mut parent = std::process::Command::new(std::env::current_exe().unwrap());
    parent
        .args(["--exact", name])
        .env("CRATE_CC_NO_DEFAULTS", "1");
    for var in [
        "CFLAGS",
        "HOST_CFLAGS",
        &format!("CFLAGS_{target}"),
        &format!("CFLAGS_{}", target.replace(['-', '.'], "_")),
    ] {
        parent.env(var, "-w");
    }
    let output = parent.output().unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        output.status.success() && stdout.contains("test result: ok. 1 passed"),
        "{stdout}"
    );
}
