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

/// The constructors that abort, each with the test that calls it. Only
/// `each_posix_constructor_prints_its_name_and_aborts` runs these tests, each in a
/// child process.
const REFUSED: [(&str, &str); 5] = [
    ("link::refused::event_loop", "UA_EventLoop_new_POSIX"),
    ("link::refused::tcp", "UA_ConnectionManager_new_POSIX_TCP"),
    ("link::refused::udp", "UA_ConnectionManager_new_POSIX_UDP"),
    (
        "link::refused::ethernet",
        "UA_ConnectionManager_new_POSIX_Ethernet",
    ),
    ("link::refused::interrupt", "UA_InterruptManager_new_POSIX"),
];

mod refused {
    use super::*;

    const EMPTY: UaString = UaString {
        length: 0,
        data: std::ptr::null_mut(),
    };

    #[test]
    #[ignore = "a child process of each_posix_constructor_prints_its_name_and_aborts"]
    fn event_loop() {
        // SAFETY: it aborts before it reads its argument.
        unsafe { UA_EventLoop_new_POSIX(std::ptr::null()) };
    }

    #[test]
    #[ignore = "a child process of each_posix_constructor_prints_its_name_and_aborts"]
    fn tcp() {
        // SAFETY: as above.
        unsafe { UA_ConnectionManager_new_POSIX_TCP(EMPTY) };
    }

    #[test]
    #[ignore = "a child process of each_posix_constructor_prints_its_name_and_aborts"]
    fn udp() {
        // SAFETY: as above.
        unsafe { UA_ConnectionManager_new_POSIX_UDP(EMPTY) };
    }

    #[test]
    #[ignore = "a child process of each_posix_constructor_prints_its_name_and_aborts"]
    fn ethernet() {
        // SAFETY: as above.
        unsafe { UA_ConnectionManager_new_POSIX_Ethernet(EMPTY) };
    }

    #[test]
    #[ignore = "a child process of each_posix_constructor_prints_its_name_and_aborts"]
    fn interrupt() {
        // SAFETY: as above.
        unsafe { UA_InterruptManager_new_POSIX(EMPTY) };
    }
}

#[test]
#[cfg(unix)]
fn each_posix_constructor_prints_its_name_and_aborts() {
    use std::os::unix::process::ExitStatusExt;
    const SIGABRT: i32 = 6;
    for (test, name) in REFUSED {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--ignored"])
            .output()
            .unwrap();
        assert_eq!(output.status.signal(), Some(SIGABRT), "{name}");
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            format!("connector-opcua: open62541 called {name}, which is not built\n")
        );
    }
}
