//! Checks that the open62541 copy, `shim.c`, and `server.c` compile and link.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use env::rng::Rng;

use crate::event::{self, Loop};
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

/// The constructors that abort, of the event loop and of each policy that the copy
/// omits, and the members of the event loop that abort.
const REFUSED: [&str; 19] = [
    "UA_EventLoop_new_POSIX",
    "UA_ConnectionManager_new_POSIX_TCP",
    "UA_ConnectionManager_new_POSIX_UDP",
    "UA_ConnectionManager_new_POSIX_Ethernet",
    "UA_InterruptManager_new_POSIX",
    "UA_SecurityPolicy_Basic128Rsa15",
    "UA_SecurityPolicy_Basic256",
    "UA_SecurityPolicy_EccBrainpoolP256r1",
    "UA_SecurityPolicy_EccBrainpoolP384r1",
    "UA_SecurityPolicy_EccCurve25519",
    "UA_SecurityPolicy_EccCurve448",
    "UA_SecurityPolicy_EccNistP256",
    "UA_SecurityPolicy_EccNistP256AesGcm",
    "UA_SecurityPolicy_EccNistP256ChaChaPoly",
    "UA_SecurityPolicy_EccNistP384",
    "UA_EventLoop.stop",
    "UA_EventLoop.free",
    "UA_EventLoop.registerEventSource",
    "UA_EventLoop.deregisterEventSource",
];

/// The constructors of the file stores, which `shim.c` defines to abort where the copy
/// names them.
const FILE_STORES: &[&str] = if cfg!(any(target_os = "linux", windows)) {
    &[
        "UA_CertificateGroup_Filestore",
        "UA_SecurityPolicy_Filestore",
    ]
} else {
    &[]
};

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
        _ => call_policy(&name),
    };
}

/// Calls the constructor of the policy or the file store that `name` names.
fn call_policy(name: &str) -> ! {
    let (none, logger) = (std::ptr::null_mut(), std::ptr::null());
    let empty = || Bytes {
        length: 0,
        data: std::ptr::null_mut(),
    };
    let rsa = |make: ffi::Rsa| {
        // SAFETY: each aborts before it reads its arguments.
        unsafe { make(none, empty(), empty(), logger) }
    };
    let ecc = |make: ffi::Ecc| {
        // SAFETY: as above.
        unsafe { make(none, 0, empty(), empty(), logger) }
    };
    let status = match name {
        "UA_SecurityPolicy_Basic128Rsa15" => rsa(ffi::UA_SecurityPolicy_Basic128Rsa15),
        "UA_SecurityPolicy_Basic256" => rsa(ffi::UA_SecurityPolicy_Basic256),
        "UA_SecurityPolicy_EccBrainpoolP256r1" => {
            ecc(ffi::UA_SecurityPolicy_EccBrainpoolP256r1)
        }
        "UA_SecurityPolicy_EccBrainpoolP384r1" => {
            ecc(ffi::UA_SecurityPolicy_EccBrainpoolP384r1)
        }
        "UA_SecurityPolicy_EccCurve25519" => ecc(ffi::UA_SecurityPolicy_EccCurve25519),
        "UA_SecurityPolicy_EccCurve448" => ecc(ffi::UA_SecurityPolicy_EccCurve448),
        "UA_SecurityPolicy_EccNistP256" => ecc(ffi::UA_SecurityPolicy_EccNistP256),
        "UA_SecurityPolicy_EccNistP256AesGcm" => {
            ecc(ffi::UA_SecurityPolicy_EccNistP256AesGcm)
        }
        "UA_SecurityPolicy_EccNistP256ChaChaPoly" => {
            ecc(ffi::UA_SecurityPolicy_EccNistP256ChaChaPoly)
        }
        "UA_SecurityPolicy_EccNistP384" => ecc(ffi::UA_SecurityPolicy_EccNistP384),
        #[cfg(any(target_os = "linux", windows))]
        // SAFETY: as above.
        "UA_CertificateGroup_Filestore" => unsafe {
            ffi::UA_CertificateGroup_Filestore(none, none, empty(), logger, none)
        },
        #[cfg(any(target_os = "linux", windows))]
        // SAFETY: as above.
        "UA_SecurityPolicy_Filestore" => unsafe {
            ffi::UA_SecurityPolicy_Filestore(none, none, empty())
        },
        _ => call_member(name),
    };
    panic!("{name} gave {:?}", Status(status))
}

/// Calls the member of the event loop that `name` names.
fn call_member(name: &str) -> ! {
    let mut sim = sim::Sim::new(sim::Config::default());
    let clock = sim.node(sim::node::Config::default()).clock();
    let events = Loop::new(event::Time::monotonic(clock), &mut Rng::from_seed(0));
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
fn each_refused_call_prints_its_name_and_aborts() {
    for name in REFUSED.iter().chain(FILE_STORES) {
        let line =
            format!("connector-opcua: open62541 called {name}, which is not built\n");
        crate::child::aborted_in_c("link::call_refused", &[(CHILD, name)], &line);
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
    crate::compiler::sim(&mut shim, &copy);
    shim.target(target)
        .host(target)
        .opt_level(3)
        .cargo_metadata(false)
        .cargo_warnings(false)
        .get_compiler()
}

/// Gives each pragma of `file` of `src/` after the preprocessor, which joins split
/// lines and expands `_Pragma`.
fn shim_pragmas(file: &str) -> Vec<String> {
    // Not `src()`, whose `\\?\` prefix on Windows GCC does not open.
    let path = std::path::Path::new(ROOT).join("src").join(file);
    let output = shim_compiler()
        .to_command()
        .arg("-E")
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    let mut main = None;
    let mut in_shim = false;
    let mut pragmas = Vec::new();
    for line in text.lines() {
        // A line marker, `# <line> "<file>" <flags>`, names the file of the next lines.
        // The first names `path`, with the escapes that each marker gives it.
        if let Some(marker) = line.strip_prefix("# ") {
            let name =
                &marker[marker.find('"').unwrap() + 1..marker.rfind('"').unwrap()];
            in_shim = *main.get_or_insert(name) == name;
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
        .args(["-S", "-o", "-", "-x", "c", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
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
fn the_pragma_check_reads_a_file_whose_path_is_not_ascii() {
    // Clang writes each byte that is not ASCII in a line marker as an octal escape, and
    // each compiler escapes a `"`, which Windows does not allow in a name.
    let name = if cfg!(unix) { "José \"q\"" } else { "José" };
    let dir = std::path::Path::new(env!("OUT_DIR")).join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("pragma.c");
    std::fs::write(&file, "#pragma GCC diagnostic push\nint x;\n").unwrap();
    assert_eq!(
        shim_pragmas(file.to_str().unwrap()),
        ["#pragma GCC diagnostic push"]
    );
}

#[test]
fn the_shim_ignores_only_the_unused_parameters_of_the_headers() {
    // `cc` reads C flags from the environment, and `-w` there hides each warning.
    if !crate::child::running() {
        let name = "link::the_shim_ignores_only_the_unused_parameters_of_the_headers";
        crate::child::run(name, &[]);
        return;
    }
    for file in ["shim.c", "server.c"] {
        assert_eq!(
            shim_pragmas(file),
            [
                "#pragma GCC diagnostic push",
                "#pragma GCC diagnostic ignored \"-Wunused-parameter\"",
                "#pragma GCC diagnostic pop",
            ],
            "{file}"
        );
    }
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
    let own = check_shim(
        "#include \"shim.h\"",
        "int in_our_header(int unused) { return 0; }",
    );
    assert!(own.contains("unused parameter 'unused'"), "{own}");
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
#[cfg(unix)]
fn the_shim_check_finds_a_cc_relative_to_the_crate() {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    let cc = Path::new(env!("OUT_DIR")).join("relative-cc");
    std::fs::write(&cc, "#!/bin/sh\nexec cc \"$@\"\n").unwrap();
    std::fs::set_permissions(&cc, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (root, cc) = (root(), cc.canonicalize().unwrap());
    let common = root.ancestors().find(|dir| cc.starts_with(dir)).unwrap();
    let up = "../".repeat(root.strip_prefix(common).unwrap().components().count());
    let relative = format!("{up}{}", cc.strip_prefix(common).unwrap().display());
    let name = "link::the_shim_ignores_only_the_unused_parameters_of_the_headers";
    crate::child::run(name, &[("CC", &relative)]);
}

#[test]
#[cfg(unix)]
fn the_shim_check_writes_no_file() {
    // Tests that run at the same time would share the file, and GCC writes it in place.
    use std::os::unix::fs::PermissionsExt;
    let cc = std::path::Path::new(env!("OUT_DIR")).join("stdout-cc");
    let script = "#!/bin/sh\nfor arg; do\n  [ \"$last\" = -o ] && [ \"$arg\" != - ] && \
                  exit 1\n  last=$arg\ndone\nexec cc \"$@\"\n";
    std::fs::write(&cc, script).unwrap();
    std::fs::set_permissions(&cc, std::fs::Permissions::from_mode(0o755)).unwrap();
    let name = "link::the_shim_ignores_only_the_unused_parameters_of_the_headers";
    crate::child::run(name, &[("CC", cc.to_str().unwrap())]);
}

#[test]
fn the_shim_check_uses_the_compiler_of_cc() {
    let name = "link::the_shim_ignores_only_the_unused_parameters_of_the_headers";
    let output = crate::child::output(name, &[("CC", "/missing/gcc")]);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("kind: NotFound"), "{stdout}");
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

/// Each symbol outside the copy, `shim.c`, `server.c`, and the libcrypto ([`PREFIX`])
/// that they may name in glibc, other than [`PLUGINS`]: on x86-64 with GCC or Clang at
/// each optimization level, and on 64-bit Arm with Clang at `-O2` and
/// `-moutline-atomics`.
const OUTSIDE: [&str; 41] = [
    "_GLOBAL_OFFSET_TABLE_",
    "__ctype_b_loc",
    "__errno_location",
    "__fprintf_chk",
    "__memcpy_chk",
    "__memmove_chk",
    "__memset_chk",
    "__printf_chk",
    "__sprintf_chk",
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
    "fputs",
    "fwrite",
    "memcmp",
    "memcpy",
    "memmove",
    "memset",
    "printf",
    "puts",
    "sprintf",
    "stderr",
    "stdout",
    "strcmp",
    "strlen",
    "strncmp",
    "strncpy",
    "strtod",
    "strtok_r",
    "syslog",
    "write",
];

/// The symbols from outside the copy that only one file of it may name, by that file:
/// `fgets` and `stdin` of `readPrivateKeyPassword`. Each is a call of the source, not
/// a name that the compiler makes, so the object of its file names it at each
/// optimization level.
const PLUGINS: [(&str, &[&str]); 1] =
    [("plugins/ua_config_default.c", &["fgets", "stdin"])];

/// The names of [`PLUGINS`] that `object` may name.
fn plugins(object: &str) -> &'static [&'static str] {
    PLUGINS
        .iter()
        .find(|(file, _)| compiled(object, file))
        .map_or(&[], |(_, names)| names)
}

/// Whether `object` is the object of the source `file`: its name ends in `-<stem>.o`,
/// as `cc` and the Arm test give it.
fn compiled(object: &str, file: &str) -> bool {
    object.ends_with(&format!("-{}.o", stem(file)))
}

/// The name of the file at `path` without its extension.
fn stem(path: &str) -> &str {
    let path = std::path::Path::new(path);
    path.file_stem().unwrap().to_str().unwrap()
}

#[test]
fn plugins_admits_each_name_only_in_the_object_of_its_file() {
    assert_eq!(
        plugins("80e9fda3d50fc3d4-ua_config_default.o"),
        ["fgets", "stdin"]
    );
    for object in [
        "80e9fda3d50fc3d4-ua_client.o",
        "ua_config_default.o",
        "1-my_ua_config_default.o",
        "2-ua_config_default.o.o",
        "3-ua_config_default.c",
    ] {
        assert!(plugins(object).is_empty(), "{object}");
    }
}

/// The prefix that the headers of the libcrypto of `aws-lc-sys` give each of its
/// symbols, with the `_` that ends it. `cargo xtask open62541` lists each that the
/// copy may name.
const PREFIX: &str = env!("CONNECTOR_OPCUA_PREFIX");

/// The include directory of the libcrypto of `aws-lc-sys`.
const LIBCRYPTO: &str = env!("CONNECTOR_OPCUA_LIBCRYPTO");

#[test]
fn the_prefix_names_a_version_of_aws_lc() {
    let version = PREFIX
        .strip_prefix("aws_lc_")
        .and_then(|v| v.strip_suffix('_'));
    let numbers = version.map(|v| v.split('_').map(str::parse::<u32>).collect());
    assert!(
        matches!(numbers, Some(Ok(n)) if Vec::len(&n) == 3),
        "{PREFIX}"
    );
}

/// At `UA_MULTITHREADING` 0 the copy takes no lock and calls no atomic, so the symbol
/// tests fail on each such name only while the list holds none.
#[test]
fn the_list_holds_no_lock_and_no_atomic() {
    let held: Vec<_> = OUTSIDE
        .iter()
        .chain(PLUGINS.iter().flat_map(|(_, names)| *names))
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

/// The symbols that `nm` with `flag` gives for each object of `files`, by the name of
/// the object: its name in an archive, or else the name of its file. No path of
/// `files` may hold `": "`.
fn objects(
    files: &[std::path::PathBuf],
    flag: &str,
) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
    let output = std::process::Command::new("nm")
        .args(["-A", "-P", flag])
        .args(files)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    let mut objects =
        std::collections::BTreeMap::<_, std::collections::BTreeSet<_>>::new();
    for line in text.lines() {
        let (file, rest) = line
            .split_once(": ")
            .unwrap_or_else(|| panic!("nm gives a line with no \": \": {line}"));
        let object = match file.strip_suffix(']').and_then(|f| f.rsplit_once('[')) {
            Some((_, member)) => member,
            None => file.rsplit('/').next().unwrap(),
        };
        let name = rest.split_whitespace().next().unwrap();
        objects
            .entry(object.to_owned())
            .or_default()
            .insert(name.to_owned());
    }
    objects
}

/// The symbols that `nm` with `flag` gives for `files`.
fn names(
    files: &[std::path::PathBuf],
    flag: &str,
) -> std::collections::BTreeSet<String> {
    objects(files, flag).into_values().flatten().collect()
}

/// The archive of this build, which holds the copy and the shim.
fn archive() -> std::path::PathBuf {
    std::path::Path::new(env!("OUT_DIR")).join("libopen62541.a")
}

/// The symbols that `nm` with `flag` gives for [`archive`].
fn symbols(flag: &str) -> std::collections::BTreeSet<String> {
    names(&[archive()], flag)
}

/// Each name that an object of `undefined` names and that neither `defined`,
/// [`OUTSIDE`], [`PLUGINS`] for that object, the libcrypto ([`PREFIX`]), nor `admitted`
/// holds, as `object: name`.
///
/// # Panics
///
/// When no object names `connector_opcua_malloc`, or a file of [`PLUGINS`] has not one
/// object that names each name that the list gives it.
fn unlisted(
    defined: &std::collections::BTreeSet<String>,
    undefined: &std::collections::BTreeMap<String, std::collections::BTreeSet<String>>,
    admitted: impl Fn(&str) -> bool,
) -> Vec<String> {
    assert!(
        undefined
            .values()
            .any(|names| names.contains("connector_opcua_malloc")),
        "no object names connector_opcua_malloc: {undefined:?}"
    );
    for (file, names) in PLUGINS {
        let named: Vec<_> = undefined
            .iter()
            .filter(|(object, _)| compiled(object, file))
            .collect();
        assert!(
            matches!(&named[..], [(_, held)] if names.iter().all(|n| held.contains(*n))),
            "{file} has no one object that names {names:?}: {named:?}"
        );
    }
    undefined
        .iter()
        .flat_map(|(object, names)| names.iter().map(move |name| (object, name)))
        .filter(|(object, name)| {
            !(defined.contains(*name)
                || OUTSIDE.contains(&name.as_str())
                || plugins(object).contains(&name.as_str())
                || name.starts_with(PREFIX)
                || admitted(name))
        })
        .map(|(object, name)| format!("{object}: {name}"))
        .collect()
}

/// Each object of `objects` with its names, as [`objects`] gives them from `nm`.
fn create_undefined(
    objects: &[(&str, &[&str])],
) -> std::collections::BTreeMap<String, std::collections::BTreeSet<String>> {
    objects
        .iter()
        .map(|(object, names)| {
            let names = names.iter().map(|n| (*n).to_owned()).collect();
            ((*object).to_owned(), names)
        })
        .collect()
}

#[test]
fn unlisted_gives_each_name_that_no_list_admits_by_its_object() {
    let defined = ["UA_Client_new".to_owned()].into();
    let crypto = format!("{PREFIX}EVP_sha256");
    let undefined = create_undefined(&[
        (
            "1-ua_config_default.o",
            &["fgets", "stdin", "UA_Client_new"],
        ),
        (
            "2-ua_client.o",
            &["fgets", "malloc", "memcpy", "__asan_report"],
        ),
        ("3-shim.o", &["connector_opcua_malloc", &crypto, "stdin"]),
    ]);
    assert_eq!(
        unlisted(&defined, &undefined, |name| name.starts_with("__asan")),
        [
            "2-ua_client.o: fgets",
            "2-ua_client.o: malloc",
            "3-shim.o: stdin"
        ]
    );
}

#[test]
#[should_panic(expected = "no object names connector_opcua_malloc")]
fn unlisted_panics_when_no_object_names_the_allocator() {
    unlisted(
        &[].into(),
        &create_undefined(&[("1-ua_config_default.o", &["fgets", "stdin"])]),
        |_| false,
    );
}

#[test]
#[should_panic(expected = "plugins/ua_config_default.c has no one object that names")]
fn unlisted_panics_when_no_object_of_a_file_of_plugins_exists() {
    unlisted(
        &[].into(),
        &create_undefined(&[("1-ua_client.o", &["connector_opcua_malloc", "fgets"])]),
        |_| false,
    );
}

#[test]
#[should_panic(expected = "plugins/ua_config_default.c has no one object that names")]
fn unlisted_panics_when_the_object_of_a_file_of_plugins_lacks_a_name() {
    unlisted(
        &[].into(),
        &create_undefined(&[(
            "1-ua_config_default.o",
            &["connector_opcua_malloc", "fgets"],
        )]),
        |_| false,
    );
}

#[test]
#[should_panic(expected = "plugins/ua_config_default.c has no one object that names")]
fn unlisted_panics_when_two_objects_have_a_file_of_plugins() {
    unlisted(
        &[].into(),
        &create_undefined(&[
            (
                "1-ua_config_default.o",
                &["connector_opcua_malloc", "fgets", "stdin"],
            ),
            ("2-ua_config_default.o", &["fgets", "stdin"]),
        ]),
        |_| false,
    );
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
    let undefined = objects(&[archive()], "--undefined-only");
    let unlisted = unlisted(&defined, &undefined, |name| cfg!(asan) && sanitizer(name));
    assert!(unlisted.is_empty(), "the C names {unlisted:?}");
}

/// The undefined symbols of the objects of `shim.c` and the test server `server.c`.
fn shim_undefined() -> std::collections::BTreeSet<String> {
    let shim: Vec<_> = std::fs::read_dir(env!("OUT_DIR"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            let path = path.to_string_lossy();
            path.ends_with("-shim.o") || path.ends_with("-server.o")
        })
        .collect();
    assert_eq!(shim.len(), 2, "{shim:?}");
    let undefined = names(&shim, "--undefined-only");
    for name in ["UA_Client_newWithConfig", "UA_Server_newWithConfig"] {
        assert!(undefined.contains(name), "{name} in {undefined:?}");
    }
    undefined
}

/// Neither the shim nor the test server takes a value from the PCG32 generator of the
/// copy, which OPEN62541 SOURCE bars for each nonce, key, and session token of
/// Foundation code.
#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs GNU nm")]
fn the_shim_draws_nothing_from_the_generator_of_the_copy() {
    let undefined = shim_undefined();
    let drawn: Vec<_> = ["UA_UInt32_random", "UA_Guid_random"]
        .into_iter()
        .filter(|name| undefined.contains(*name))
        .collect();
    assert!(drawn.is_empty(), "the shim calls {drawn:?}");
}

/// The symbol checks admit the libcrypto ([`PREFIX`]) in each object, and `cargo xtask
/// open62541` admits each call of it only in the files of the copy that it keys.
/// Neither the shim nor the test server names the libcrypto, so neither calls aws-lc
/// unseen.
#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs GNU nm")]
fn the_shim_names_no_libcrypto() {
    let named: Vec<_> = shim_undefined()
        .into_iter()
        .filter(|name| name.starts_with(PREFIX))
        .collect();
    assert!(named.is_empty(), "the shim names {named:?}");
}

/// The directory of the crate.
fn root() -> std::path::PathBuf {
    std::path::Path::new(ROOT).canonicalize().unwrap()
}

/// The `src/` directory of the crate.
fn src() -> std::path::PathBuf {
    root().join("src")
}

/// The cfg of the code that only tests, benchmarks, and fuzz targets build, which the
/// scan counts as test code.
const SIM: &str = "feature=\"sim\"";

/// Each identifier for which `named` holds, as `path: identifier`, in the Rust
/// outside tests of the crate at `root`: each `.rs` file outside `tests/` and each
/// file that a build of the library reads ([`built`]), less each file that a build
/// with [`SIM`] on reads ([`tested`]) and no build of the library reads. Comments
/// and inline modules count.
fn named_outside_tests(
    root: &std::path::Path,
    named: impl Fn(&str) -> bool,
) -> Vec<String> {
    let (library, _) = built(root, &[]);
    let tested = tested(root);
    let tests = root.join("tests");
    let mut files = files_under(root, "rs");
    files.retain(|path| !path.starts_with(&tests));
    files.extend(library.iter().cloned());
    files.sort();
    files.dedup();
    files
        .into_iter()
        .filter(|path| library.contains(path) || !tested.contains(path))
        .flat_map(|path| named_in(&path, &named))
        .collect()
}

/// Each file that a build of the crate at `root` with [`SIM`] on reads, sorted: with
/// `test` off, and with the test harness.
fn tested(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let sim = on(&[SIM.to_owned()]);
    let (mut files, _) = built(root, &sim);
    // rustc refuses `--test` with `-Cpanic=abort`, but still gives the files it read.
    files.extend(built(root, &[vec!["--test".to_owned()], sim].concat()).0);
    files.sort();
    files.dedup();
    files
}

/// Each file that a build of the library of the crate at `root` with the flags `base`
/// reads, and the cfgs that the builds set, sorted. The builds are those on this host,
/// with the target features of this build, and with each set of the cfgs that the
/// crate tests and rustc does not know (features among them, less [`SIM`], which only
/// `base` sets), each with debug assertions on and off and with each panic strategy.
/// rustc names each such cfg in a build that reads the file that tests it. A file
/// under a known value of a target cfg that this build does not set, such as
/// `target_os = "windows"`, is read by no build.
///
/// # Panics
///
/// On a value that rustc does not know of a cfg other than `feature`, since the scan
/// cannot know which values a build gives it.
fn built(
    root: &std::path::Path,
    base: &[String],
) -> (Vec<std::path::PathBuf>, Vec<String>) {
    let mut cfgs: Vec<String> = Vec::new();
    let mut runs = std::collections::BTreeSet::new();
    let mut files = Vec::new();
    loop {
        let mut known = cfgs.clone();
        known.push(SIM.to_owned());
        let mut found = Vec::new();
        for set in 0..1usize << cfgs.len() {
            let set: Vec<_> = (0..cfgs.len())
                .filter(|bit| set >> bit & 1 == 1)
                .map(|bit| cfgs[bit].clone())
                .collect();
            for assertions in ["on", "off"] {
                for panic in ["unwind", "abort"] {
                    let mut flags = base.to_vec();
                    flags.extend(on(&set));
                    flags.push(format!("-Cdebug-assertions={assertions}"));
                    flags.push(format!("-Cpanic={panic}"));
                    if runs.insert(flags.clone()) {
                        let (read, unknown) = read_by_rustc(root, &known, &flags);
                        files.extend(read);
                        found.extend(unknown);
                    }
                }
            }
        }
        if found.is_empty() {
            files.sort();
            files.dedup();
            return (files, cfgs);
        }
        cfgs.extend(found);
        cfgs.sort();
        cfgs.dedup();
    }
}

/// The flags of rustc that set each cfg of `cfgs`.
fn on(cfgs: &[String]) -> Vec<String> {
    cfgs.iter()
        .flat_map(|cfg| ["--cfg".to_owned(), cfg.clone()])
        .collect()
}

/// Each file that rustc reads to expand the library of the crate at `root`, at
/// `src/lib.rs`, with `flags`, and each cfg that the crate tests, rustc does not know,
/// and `known` does not hold, as `name` or `feature="name"`. No other crate is
/// given, so its names fail to resolve, but rustc lists the files of the crate still.
///
/// # Panics
///
/// On a value that rustc does not know of a cfg other than `feature`.
fn read_by_rustc(
    root: &std::path::Path,
    known: &[String],
    flags: &[String],
) -> (Vec<std::path::PathBuf>, Vec<String>) {
    let (features, names): (Vec<_>, Vec<_>) =
        known.iter().partition(|cfg| cfg.starts_with("feature="));
    let values: Vec<_> = features
        .iter()
        .map(|feature| &feature["feature=".len()..])
        .collect();
    let mut checked = vec![
        "cfg(test)".to_owned(),
        format!("cfg(feature, values({}))", values.join(",")),
    ];
    checked.extend(names.iter().map(|name| format!("cfg({name})")));
    let features = env!("CONNECTOR_OPCUA_TARGET_FEATURES").split(',');
    let features: Vec<_> = features.map(|feature| format!("+{feature}")).collect();
    let features = features.join(",");
    // rustc checks cfgs only when it emits more than dep-info.
    let out = scan_out();
    // rustc makes no directory when it stops at a parse error.
    std::fs::create_dir_all(&out).unwrap();
    let output = std::process::Command::new("rustc")
        .args(["--edition", "2024", "--crate-type", "lib"])
        .arg(format!("-Ctarget-feature={features}"))
        .args(["--emit", "dep-info=-,metadata", "--out-dir"])
        .arg(&out)
        .args(checked.iter().flat_map(|cfg| ["--check-cfg", cfg]))
        .args(["--force-warn", "unexpected_cfgs"])
        .args(flags)
        .arg(root.join("src/lib.rs"))
        .output()
        .unwrap();
    std::fs::remove_dir_all(&out).unwrap();
    let deps = String::from_utf8(output.stdout).unwrap();
    let errors = String::from_utf8(output.stderr).unwrap();
    assert!(!deps.is_empty(), "rustc gives no files: {errors}");
    let files = deps
        .lines()
        .filter_map(|line| line.strip_suffix(':'))
        .map(|path| std::fs::canonicalize(path.replace("\\ ", " ")).unwrap())
        .collect();
    (files, unknown_cfgs(&errors))
}

/// The directory of the metadata that [`read_by_rustc`] writes, one for each thread.
fn scan_out() -> std::path::PathBuf {
    let thread = std::thread::current().id();
    std::path::Path::new(env!("OUT_DIR"))
        .join("scan-out")
        .join(format!("{}-{thread:?}", std::process::id()))
}

/// Each cfg that the warnings of rustc in `errors` give as unknown. Only the head and
/// the notes of a warning count, since the source lines that it quotes may hold any
/// text.
///
/// # Panics
///
/// On an unknown value of a cfg other than `feature`.
fn unknown_cfgs(errors: &str) -> Vec<String> {
    let quoted = |line: &str, start: &str, before: &str| {
        let rest = line.trim_start().strip_prefix(start)?;
        let (_, rest) = rest.split_once(before)?;
        Some(rest.split_once('`')?.0.to_owned())
    };
    let head = |line: &str, kind: &str| {
        quoted(
            line,
            "warning: unexpected `cfg` condition ",
            &format!("{kind}: `"),
        )
    };
    let lines: Vec<_> = errors.lines().collect();
    let mut cfgs = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        if let Some(name) = head(line, "name") {
            cfgs.push(name);
        } else if let Some(value) = head(line, "value") {
            let mut notes = lines[at..].iter();
            let of = notes.find_map(|line| quoted(line, "= note: ", " for `"));
            let of = of.unwrap_or_else(|| panic!("rustc names no cfg: {errors}"));
            assert!(
                of == "feature",
                "the scan cannot build each value of the cfg `{of}`, such as `{value}`"
            );
            cfgs.push(format!("feature=\"{value}\""));
        }
    }
    cfgs
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

/// Each identifier of `path` for which `named` holds, as `path:line: identifier`. A
/// byte that is not UTF-8 splits identifiers.
#[expect(
    clippy::disallowed_methods,
    reason = "a test reads the files of the crate"
)]
fn named_in(path: &std::path::Path, named: impl Fn(&str) -> bool) -> Vec<String> {
    let text = String::from_utf8_lossy(&std::fs::read(path).unwrap()).into_owned();
    let lines = (1..).zip(text.lines());
    let found = lines.flat_map(|(line, text)| {
        words(text)
            .filter(|word| named(word))
            .map(move |word| (line, word))
    });
    found
        .map(|(line, word)| format!("{}:{line}: {word}", path.display()))
        .collect()
}

/// The scan names `shim_client_new` in each file outside tests that names it, and in
/// no test file.
#[test]
fn the_scan_names_only_the_files_outside_tests() {
    let client = |name: &str| name == "shim_client_new";
    let named = named_outside_tests(&root(), client);
    let found = [
        named_in(&src().join("client.rs"), client),
        named_in(&src().join("ffi.rs"), client),
    ]
    .concat();
    assert_eq!(found.len(), 3, "{found:?}");
    assert_eq!(named, found);
}

/// A file that only a test build reads, also with [`SIM`] on, is cut off, with the
/// files that it declares.
#[test]
fn the_scan_cuts_off_only_the_files_of_a_test_build() {
    let root = create_tree(
        "gated",
        &[
            (
                "src/lib.rs",
                "#[cfg(test)]\n#[allow(unused)]\nmod t;\nmod p;\n\
                 #[cfg(feature = \"sim\")]\nmod s;\n",
            ),
            ("src/t.rs", "mod u;"),
            ("src/s.rs", "fn named() {}"),
            ("src/t/u.rs", "fn named() {}"),
            ("src/p.rs", "fn named() {}"),
        ],
    );
    let named = named_outside_tests(&root, |name| name == "named");
    assert_eq!(named, [at(&root, "src/p.rs")]);
}

/// A file that a build of the library reads is not cut off when a test build reads it
/// too: with a feature off or on, no debug assertions, panics that abort, or a cfg of
/// the crate, also one that only such a build tests.
#[test]
fn the_scan_cuts_off_no_file_of_a_build_of_the_library() {
    let root = create_tree(
        "built",
        &[
            (
                "src/lib.rs",
                "#[cfg(any(test, not(feature = \"sim\")))]\nmod f;\n\
                 #[cfg(any(test, not(debug_assertions)))]\nmod d;\n\
                 #[cfg(any(test, panic = \"abort\"))]\nmod p;\n\
                 #[cfg(any(test, feature = \"foo\"))]\nmod o;\n\
                 #[cfg(any(test, outer))]\nmod a;\n",
            ),
            ("src/f.rs", "fn named() {}"),
            ("src/d.rs", "fn named() {}"),
            ("src/p.rs", "fn named() {}"),
            ("src/o.rs", "fn named() {}"),
            ("src/a.rs", "#[cfg(any(test, inner))]\nmod i;\n"),
            ("src/a/i.rs", "fn named() {}"),
        ],
    );
    let named = named_outside_tests(&root, |name| name == "named");
    let files = ["src/a/i.rs", "src/d.rs", "src/f.rs", "src/o.rs", "src/p.rs"];
    assert_eq!(named, files.map(|file| at(&root, file)));
}

/// The scan reads a file outside `src/` that a `path` attribute, also under
/// `cfg_attr`, or an `include!` gives to the crate, and cuts off the file that the
/// test build reads in its place.
#[test]
fn the_scan_reads_each_file_that_the_crate_includes() {
    let root = create_tree(
        "path",
        &[
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
        &[(
            "src/lib.rs",
            "pub static B: &[u8] = include_bytes!(\"../b.der\");\n",
        )],
    );
    std::fs::write(root.join("b.der"), b"\xffnamed\xfe").unwrap();
    let named = named_outside_tests(&root, |name| name == "named");
    assert_eq!(named, [at(&root, "b.der")]);
}

/// The scan refuses a value that rustc does not know of a cfg other than a feature:
/// it cannot know which values a build gives it.
#[test]
fn the_scan_refuses_a_cfg_value_that_it_cannot_build() {
    for (cfg, of, value) in [
        ("foo = \"bar\"", "foo", "bar"),
        ("target_os = \"nope\"", "target_os", "nope"),
    ] {
        let lib = format!("#[cfg({cfg})]\nmod f;\n");
        let root = create_tree("value", &[("src/lib.rs", &lib), ("src/f.rs", "")]);
        let refused =
            std::panic::catch_unwind(|| named_outside_tests(&root, |_| false));
        let message = *refused.unwrap_err().downcast::<String>().unwrap();
        let expected = format!(
            "the scan cannot build each value of the cfg `{of}`, such as `{value}`"
        );
        assert_eq!(message, expected);
    }
}

/// The scan finds a cfg of the crate under an `allow` or `expect` of `unexpected_cfgs`,
/// of the crate or of an item.
#[test]
fn the_scan_finds_a_cfg_that_an_allow_hides() {
    for (case, lib) in [
        ("crate", "#![allow(unexpected_cfgs)]\nmod m;\n"),
        ("item", "#[allow(unexpected_cfgs)]\nmod m;\n"),
        (
            "expect",
            "#![expect(unexpected_cfgs, reason = \"r\")]\nmod m;\n",
        ),
    ] {
        let root = create_tree(
            case,
            &[
                ("src/lib.rs", lib),
                ("src/m.rs", "#[cfg(any(test, outer))]\nmod a;\n"),
                ("src/m/a.rs", "fn named() {}"),
            ],
        );
        let named = named_outside_tests(&root, |name| name == "named");
        assert_eq!(named, [at(&root, "src/m/a.rs")], "{case}");
    }
}

/// The scan reads the cfg of a warning from its head and its notes, not from the
/// source lines that it quotes.
#[test]
fn the_scan_reads_no_cfg_from_a_quoted_comment() {
    let lib = concat!(
        "#[cfg(any(test, feature = \"foo\"))] // A stand-in for `Clock`, ",
        "not warning: unexpected `cfg` condition name: `ghost`.\nmod o;\n",
    );
    let root = create_tree(
        "quoted",
        &[("src/lib.rs", lib), ("src/o.rs", "fn named() {}")],
    );
    let named = named_outside_tests(&root, |name| name == "named");
    assert_eq!(named, [at(&root, "src/o.rs")]);
    assert_eq!(built(&root, &[]).1, ["feature=\"foo\""]);
    let lib = "#[cfg(target_os = \"nope\")] // = note: as for `feature` x\nmod f;\n";
    let root = create_tree("hidden", &[("src/lib.rs", lib), ("src/f.rs", "")]);
    let refused = std::panic::catch_unwind(|| named_outside_tests(&root, |_| false));
    let message = *refused.unwrap_err().downcast::<String>().unwrap();
    let expected =
        "the scan cannot build each value of the cfg `target_os`, such as `nope`";
    assert_eq!(message, expected);
}

/// The builds of the library have the target features of this build, also those that
/// a flag of the build turns on.
#[test]
fn the_scan_builds_with_the_target_features_of_the_build() {
    let features = env!("CONNECTOR_OPCUA_TARGET_FEATURES").split(',');
    let features: Vec<_> = features
        .map(|feature| format!("target_feature = \"{feature}\""))
        .collect();
    let lib = format!("#[cfg(any(test, all({})))]\nmod f;\n", features.join(", "));
    let root = create_tree(
        "feature",
        &[("src/lib.rs", &lib), ("src/f.rs", "fn named() {}")],
    );
    let named = named_outside_tests(&root, |name| name == "named");
    assert_eq!(named, [at(&root, "src/f.rs")]);
}

/// The builds of the library set each cfg that the crate tests and rustc does not
/// know, less [`SIM`], and only the builds with [`SIM`] on read the code behind it.
#[test]
fn the_scan_builds_each_cfg_of_the_crate() {
    let (library, cfgs) = built(&root(), &[]);
    assert_eq!(cfgs, ["asan", "feature=\"open62541\""]);
    let bench = src().join("bench.rs");
    assert!(!library.contains(&bench), "{library:?}");
    let tested = tested(&root());
    assert!(tested.contains(&bench), "{tested:?}");
}

/// A test mock that a `path` attribute gives in place of a module is cut off, and
/// the module is read.
#[test]
fn the_scan_reads_the_module_that_a_test_mock_shadows() {
    let root = create_tree(
        "mock",
        &[
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

/// A file that only a test build reads is cut off also under a cfg that only test
/// code tests, or under debug assertions off or panics that abort.
#[test]
fn the_scan_cuts_off_the_files_of_each_test_build() {
    for (parent, at) in [("test", "t"), ("feature = \"sim\"", "s")] {
        for (case, cfg) in [
            ("feature", "feature = \"bar\""),
            ("name", "outer"),
            ("assertions", "not(debug_assertions)"),
            ("abort", "panic = \"abort\""),
        ] {
            let lib = format!("#[cfg({parent})]\nmod m;\n");
            let m = format!("#[cfg({cfg})]\nmod b;\n");
            let root = create_tree(
                &format!("tested-{at}-{case}"),
                &[
                    ("src/lib.rs", &lib),
                    ("src/m.rs", &m),
                    ("src/m/b.rs", "fn named() {}"),
                ],
            );
            let named = named_outside_tests(&root, |name| name == "named");
            assert_eq!(named, Vec::<String>::new(), "{parent}, {case}");
        }
    }
}

/// A file behind [`SIM`] that only a build without `test` reads, as of a benchmark or
/// a fuzz target, is cut off.
#[test]
fn the_scan_cuts_off_the_sim_files_of_a_build_without_test() {
    for (case, lib, b) in [
        (
            "nested",
            "#[cfg(feature = \"sim\")]\nmod m;\n",
            "src/m/b.rs",
        ),
        (
            "all",
            "#[cfg(all(feature = \"sim\", not(test)))]\nmod b;\n",
            "src/b.rs",
        ),
    ] {
        let root = create_tree(
            &format!("untested-{case}"),
            &[
                ("src/lib.rs", lib),
                ("src/m.rs", "#[cfg(not(test))]\nmod b;\n"),
                (b, "fn named() {}"),
            ],
        );
        let named = named_outside_tests(&root, |name| name == "named");
        assert_eq!(named, Vec::<String>::new(), "{case}");
    }
}

/// A file that only a test function reads is cut off.
#[test]
fn the_scan_cuts_off_the_files_of_a_test_function() {
    let root = create_tree(
        "test-function",
        &[
            (
                "src/lib.rs",
                "#[test]\nfn t() {\n    let _: () = include!(\"../c.rs\");\n}\n",
            ),
            ("c.rs", "{ fn named() {} }"),
        ],
    );
    let named = named_outside_tests(&root, |name| name == "named");
    assert_eq!(named, Vec::<String>::new());
}

/// A scan leaves no output of rustc behind.
#[test]
fn the_scan_removes_its_output() {
    let root = create_tree("output", &[("src/lib.rs", "fn named() {}")]);
    read_by_rustc(&root, &[], &[]);
    assert!(!scan_out().exists(), "{}", scan_out().display());
}

/// A crate that rustc cannot parse fails the scan with the errors of rustc.
#[test]
fn the_scan_of_a_crate_that_does_not_parse_gives_the_errors_of_rustc() {
    let root = create_tree("unparsed", &[("src/lib.rs", "fn named( {}")]);
    let refused = std::panic::catch_unwind(|| read_by_rustc(&root, &[], &[]));
    let message = *refused.unwrap_err().downcast::<String>().unwrap();
    assert!(message.starts_with("rustc gives no files: "), "{message}");
    let file = format!("{}:1:", root.join("src/lib.rs").display());
    assert!(message.contains(&file), "{message}");
    assert!(!scan_out().exists(), "{}", scan_out().display());
}

/// How [`named_outside_tests`] gives the name `named` on line 1 of `file` of `root`.
fn at(root: &std::path::Path, file: &str) -> String {
    format!("{}:1: named", root.join(file).display())
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

/// Each identifier for which `named` holds in the production code of the crate at
/// `root`, as [`named_in`] gives it: the Rust of [`named_outside_tests`], and each `.c`
/// and `.h` file of `src/` but the test server `src/server.c`, which only a build with
/// `sim` compiles.
fn named_in_production(
    root: &std::path::Path,
    named: impl Fn(&str) -> bool,
) -> Vec<String> {
    let src = root.join("src");
    let server = src.join("server.c");
    let mut found = named_outside_tests(root, &named);
    for path in [files_under(&src, "c"), files_under(&src, "h")].concat() {
        if path != server {
            found.extend(named_in(&path, &named));
        }
    }
    found
}

/// The production code builds no server and names no `UA_random_seed`, so it reaches
/// none of `setDefaultConfig`, `interruptServer`, and `UA_random_seed`, which `cargo
/// xtask open62541` lets call the global clock.
#[test]
fn the_production_code_builds_no_server() {
    let named = named_in_production(&root(), |name| {
        name.starts_with("UA_Server")
            || ["shim_server_new", "UA_random_seed"].contains(&name)
    });
    assert!(named.is_empty(), "the production code names {named:?}");
}

#[test]
fn the_production_scan_reads_the_c_but_the_server_file() {
    let root = create_tree(
        "production",
        &[
            ("src/lib.rs", "fn named() {}"),
            (
                "src/shim.c",
                "UA_Client *shim_client_new(UA_EventLoop *el) {\n    \
                 UA_ServerConfig_setMinimal(&config, 4840, NULL);\n",
            ),
            ("src/server.c", "UA_Server *shim_server_new(void);"),
            ("src/alloc.h", "UA_Server *server;"),
        ],
    );
    let server = |name: &str| name.starts_with("UA_Server") || name == "named";
    assert_eq!(
        named_in_production(&root, server),
        [
            at(&root, "src/lib.rs"),
            format!(
                "{}:2: UA_ServerConfig_setMinimal",
                root.join("src/shim.c").display()
            ),
            format!("{}:1: UA_Server", root.join("src/alloc.h").display()),
        ]
    );
}

/// `UA_Client_new` gives a client the stdout logger, whose `UA_Log_Stdout_log`
/// `cargo xtask open62541` lets call the global clock. The production code names
/// neither `UA_Client_new` nor a `UA_Log_Stdout` function. `shim_client_new` gives a
/// client the logger of its loop, and the stderr tests of `event` check that.
#[test]
fn the_production_code_gives_no_client_the_stdout_logger() {
    let named = named_in_production(&root(), |name| {
        name == "UA_Client_new" || name.starts_with("UA_Log_Stdout")
    });
    assert!(named.is_empty(), "the production code names {named:?}");
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

/// The copy compiles the policy `None` and the three current policies, and no
/// deprecated or ECC policy.
#[test]
fn the_copy_holds_only_the_three_current_policies() {
    let copy = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../patches/open62541");
    let sources = std::fs::read_to_string(copy.join("sources.txt")).unwrap();
    let policies: Vec<_> = sources
        .lines()
        .filter(|source| source.contains("securitypolicy_"))
        .collect();
    assert_eq!(
        policies,
        [
            "plugins/crypto/openssl/securitypolicy_aes128sha256rsaoaep.c",
            "plugins/crypto/openssl/securitypolicy_aes256sha256rsapss.c",
            "plugins/crypto/openssl/securitypolicy_basic256sha256.c",
            "plugins/crypto/openssl/securitypolicy_common.c",
            "plugins/crypto/ua_securitypolicy_none.c",
        ]
    );
}

/// The name of the stub of `shim.c` that `line` defines or refuses:
/// `REFUSE_RSA(name)`, `REFUSE_ECC(name)`, `UA_StatusCode name(`, or `refuse("name");`.
fn stub(line: &str) -> Option<&str> {
    let refused = line.strip_prefix("REFUSE_RSA(");
    let refused = refused.or_else(|| line.strip_prefix("REFUSE_ECC("));
    let defined = || line.strip_prefix("UA_StatusCode ")?.split_once('(');
    let named = || line.trim().strip_prefix("refuse(\"")?.strip_suffix("\");");
    refused
        .and_then(|rest| rest.strip_suffix(')'))
        .or_else(|| defined().map(|(name, _)| name))
        .or_else(named)
}

/// Checks the reasons of the clock keys of `encryptUserIdentityTokenEcc` and
/// `openSSL_verifyChain` in `cargo xtask open62541`: the default config of a client
/// has only the policy `None`, and the production code names no security policy, no
/// certificate group, no certificate function, and no function that adds one, other
/// than at the stubs of `shim.c` that abort.
#[test]
fn the_production_code_gives_a_client_no_security_policy() {
    let policy = |name: &str| {
        name.starts_with("UA_SecurityPolicy_")
            || name.starts_with("UA_CertificateGroup_")
            || name.starts_with("UA_CertificateUtils_")
            || [
                "UA_ClientConfig_setDefaultEncryption",
                "UA_ClientConfig_setAuthenticationCert",
                "securityPolicies",
                "authSecurityPolicies",
            ]
            .contains(&name)
    };
    let shim = root().join("src/shim.c");
    let text = std::fs::read_to_string(&shim).unwrap();
    let stubs: Vec<_> = (1..)
        .zip(text.lines())
        .filter_map(|(line, text)| Some((line, stub(text).filter(|n| policy(n))?)))
        .collect();
    let names: std::collections::BTreeSet<_> = stubs.iter().map(|(_, n)| *n).collect();
    let refused = REFUSED.iter().filter(|name| policy(name));
    let mut expected: std::collections::BTreeSet<_> = refused.copied().collect();
    expected.extend([
        "UA_CertificateGroup_Filestore",
        "UA_SecurityPolicy_Filestore",
    ]);
    assert_eq!(names, expected);
    let at_stubs: Vec<_> = stubs
        .iter()
        .map(|(line, name)| format!("{}:{line}: {name}", shim.display()))
        .collect();
    assert_eq!(named_in_production(&root(), policy), at_stubs);
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
    let files: Vec<_> = sources
        .lines()
        .enumerate()
        .map(|(i, source)| {
            let text = out.join(format!("{i}.i"));
            let object = out.join(format!("{i}-{}.o", stem(source)));
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
            preprocess.arg(format!("-I{LIBCRYPTO}"));
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
    let undefined = objects(&files, "--undefined-only");
    let unlisted = unlisted(&defined, &undefined, |_| false);
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
