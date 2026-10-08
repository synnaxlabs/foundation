//! Checks that the open62541 copy and `shim.c` compile and link.

#![expect(unsafe_code, reason = "open62541 is a C library")]

use std::ffi::{CStr, c_char, c_void};

unsafe extern "C" {
    fn UA_StatusCode_name(code: u32) -> *const c_char;
    fn UA_DateTime_now() -> i64;
    fn UA_DateTime_nowMonotonic() -> i64;
    fn UA_DateTime_localTimeUtcOffset() -> i64;
}

fn name(code: u32) -> &'static str {
    // SAFETY: `UA_StatusCode_name` takes any code and gives a static C string.
    let name = unsafe { UA_StatusCode_name(code) };
    // SAFETY: the string is static, and ends with a NUL.
    unsafe { CStr::from_ptr(name) }.to_str().unwrap()
}

#[test]
fn the_copy_names_a_status_code() {
    assert_eq!(name(0), "Good");
    assert_eq!(name(0x8034_0000), "BadNodeIdUnknown");
}

#[test]
fn the_global_clocks_give_a_fixed_time() {
    // SAFETY: each takes no argument and reads no state.
    let now = unsafe { UA_DateTime_now() };
    // SAFETY: as above.
    let monotonic = unsafe { UA_DateTime_nowMonotonic() };
    // SAFETY: as above.
    let offset = unsafe { UA_DateTime_localTimeUtcOffset() };
    assert_eq!((now, monotonic, offset), (0, 0, 0));
}

#[repr(C)]
struct UaString {
    length: usize,
    data: *mut u8,
}

unsafe extern "C" {
    fn UA_EventLoop_new_POSIX(logger: *const c_void) -> *mut c_void;
    fn UA_ConnectionManager_new_POSIX_TCP(name: UaString) -> *mut c_void;
    fn UA_ConnectionManager_new_POSIX_UDP(name: UaString) -> *mut c_void;
    fn UA_ConnectionManager_new_POSIX_Ethernet(name: UaString) -> *mut c_void;
    fn UA_InterruptManager_new_POSIX(name: UaString) -> *mut c_void;
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
    let empty = || UaString {
        length: 0,
        data: std::ptr::null_mut(),
    };
    match name.as_str() {
        // SAFETY: it aborts before it reads its argument.
        "UA_EventLoop_new_POSIX" => unsafe { UA_EventLoop_new_POSIX(std::ptr::null()) },
        // SAFETY: as above.
        "UA_ConnectionManager_new_POSIX_TCP" => unsafe {
            UA_ConnectionManager_new_POSIX_TCP(empty())
        },
        // SAFETY: as above.
        "UA_ConnectionManager_new_POSIX_UDP" => unsafe {
            UA_ConnectionManager_new_POSIX_UDP(empty())
        },
        // SAFETY: as above.
        "UA_ConnectionManager_new_POSIX_Ethernet" => unsafe {
            UA_ConnectionManager_new_POSIX_Ethernet(empty())
        },
        // SAFETY: as above.
        "UA_InterruptManager_new_POSIX" => unsafe {
            UA_InterruptManager_new_POSIX(empty())
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

unsafe extern "C" {
    fn UA_Timer_init(timer: *mut c_void);
    fn UA_Timer_next(timer: *mut c_void) -> i64;
    fn UA_Timer_process(timer: *mut c_void, now: i64) -> i64;
    fn UA_Timer_remove(timer: *mut c_void, key: u64);
    fn UA_Timer_clear(timer: *mut c_void);
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
