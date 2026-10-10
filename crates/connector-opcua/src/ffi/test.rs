use std::ffi::{c_int, c_void};
use std::mem::offset_of;

use super::{Bytes, ConnectionCallback, ConnectionManager, EventLoop, KeyValueMap, at};

/// `UA_Server`, which Rust holds only by pointer.
#[repr(C)]
pub(crate) struct Server([u8; 0]);

/// `UA_LifecycleState` of a server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct Lifecycle(pub(crate) c_int);

impl Lifecycle {
    pub(crate) const STOPPED: Self = Self(0);
    pub(crate) const STOPPING: Self = Self(2);
}

/// The members of `UA_ConnectionManager`. `shim.c` asserts the same size and
/// offsets.
#[repr(C)]
pub(crate) struct Members {
    pub(crate) next: *mut c_void,
    pub(crate) kind: c_int,
    pub(crate) name: Bytes,
    pub(crate) event_loop: *mut EventLoop,
    pub(crate) params: KeyValueMap,
    pub(crate) state: c_int,
    pub(crate) start: unsafe extern "C" fn(cm: *mut ConnectionManager) -> u32,
    pub(crate) stop: unsafe extern "C" fn(cm: *mut ConnectionManager),
    pub(crate) free: unsafe extern "C" fn(cm: *mut ConnectionManager) -> u32,
    pub(crate) protocol: Bytes,
    pub(crate) open: unsafe extern "C" fn(
        cm: *mut ConnectionManager,
        params: *const KeyValueMap,
        application: *mut c_void,
        context: *mut c_void,
        callback: ConnectionCallback,
    ) -> u32,
    pub(crate) send: unsafe extern "C" fn(
        cm: *mut ConnectionManager,
        id: usize,
        params: *const KeyValueMap,
        buffer: *mut Bytes,
    ) -> u32,
    pub(crate) close:
        unsafe extern "C" fn(cm: *mut ConnectionManager, id: usize) -> u32,
    pub(crate) alloc: unsafe extern "C" fn(
        cm: *mut ConnectionManager,
        id: usize,
        buffer: *mut Bytes,
        size: usize,
    ) -> u32,
    pub(crate) free_buffer:
        unsafe extern "C" fn(cm: *mut ConnectionManager, id: usize, buffer: *mut Bytes),
}

const _: () = {
    assert!(
        size_of::<Members>() == at(18),
        "UA_ConnectionManager changed"
    );
    assert!(offset_of!(Members, kind) == at(1), "kind moved");
    assert!(offset_of!(Members, event_loop) == at(4), "event_loop moved");
    assert!(offset_of!(Members, state) == at(7), "state moved");
    assert!(offset_of!(Members, protocol) == at(11), "protocol moved");
    assert!(offset_of!(Members, open) == at(13), "open moved");
    assert!(offset_of!(Members, send) == at(14), "send moved");
    assert!(offset_of!(Members, close) == at(15), "close moved");
    assert!(offset_of!(Members, alloc) == at(16), "alloc moved");
    assert!(
        offset_of!(Members, free_buffer) == at(17),
        "free_buffer moved"
    );
};

/// `UA_NodeId` with a numeric identifier.
#[repr(C, align(8))]
pub(crate) struct NodeId {
    pub(crate) namespace: u16,
    pub(crate) kind: c_int,
    pub(crate) numeric: u32,
    pub(crate) rest: [u32; 3],
}

const _: () = {
    assert!(size_of::<NodeId>() == at(3), "UA_NodeId changed");
    assert!(offset_of!(NodeId, kind) == 4, "kind moved");
    assert!(offset_of!(NodeId, numeric) == at(1), "numeric moved");
};

/// `UA_QualifiedName`.
#[repr(C)]
pub(crate) struct QualifiedName {
    pub(crate) namespace: u16,
    pub(crate) name: Bytes,
}

const _: () = {
    assert!(
        size_of::<QualifiedName>() == at(3),
        "UA_QualifiedName changed"
    );
    assert!(offset_of!(QualifiedName, name) == at(1), "name moved");
};

unsafe extern "C" {
    pub(crate) fn UA_DateTime_now() -> i64;
    pub(crate) fn UA_DateTime_nowMonotonic() -> i64;
    pub(crate) fn UA_DateTime_localTimeUtcOffset() -> i64;

    pub(crate) fn UA_EventLoop_new_POSIX(logger: *const c_void) -> *mut c_void;
    pub(crate) fn UA_ConnectionManager_new_POSIX_TCP(name: Bytes) -> *mut c_void;
    pub(crate) fn UA_ConnectionManager_new_POSIX_UDP(name: Bytes) -> *mut c_void;
    pub(crate) fn UA_ConnectionManager_new_POSIX_Ethernet(name: Bytes) -> *mut c_void;
    pub(crate) fn UA_InterruptManager_new_POSIX(name: Bytes) -> *mut c_void;

    pub(crate) fn UA_Client_connectAsync(
        client: *mut super::Client,
        url: *const std::ffi::c_char,
    ) -> u32;
    pub(crate) fn UA_Client_connectSecureChannelAsync(
        client: *mut super::Client,
        url: *const std::ffi::c_char,
    ) -> u32;
    pub(crate) fn __UA_Client_AsyncService(
        client: *mut super::Client,
        request: *const c_void,
        request_type: *const c_void,
        callback: *const c_void,
        response_type: *const c_void,
        data: *mut c_void,
        key: *mut u32,
    ) -> u32;

    pub(crate) fn UA_new(kind: *const c_void) -> *mut c_void;
    pub(crate) fn UA_delete(value: *mut c_void, kind: *const c_void);
    pub(crate) fn UA_findDataType(id: *const NodeId) -> *const c_void;
    pub(crate) fn UA_KeyValueMap_setScalar(
        map: *mut KeyValueMap,
        key: QualifiedName,
        value: *const c_void,
        kind: *const c_void,
    ) -> u32;
    pub(crate) fn UA_KeyValueMap_clear(map: *mut KeyValueMap);
    pub(crate) fn shim_map_set_strings(
        map: *mut KeyValueMap,
        key: *const std::ffi::c_char,
        strings: *const *const std::ffi::c_char,
        size: usize,
    ) -> u32;
    pub(crate) fn UA_KeyValueMap_getScalar(
        map: *const KeyValueMap,
        key: QualifiedName,
        kind: *const c_void,
    ) -> *const c_void;
    pub(crate) fn UA_Client_disconnect(client: *mut super::Client) -> u32;
    pub(crate) fn UA_Client_disconnectAsync(client: *mut super::Client) -> u32;

    pub(crate) fn shim_server_new(
        el: *mut EventLoop,
        port: u16,
        url: *const std::ffi::c_char,
    ) -> *mut Server;
    pub(crate) fn UA_Server_run_startup(server: *mut Server) -> u32;
    pub(crate) fn UA_Server_run_shutdown(server: *mut Server) -> u32;
    pub(crate) fn UA_Server_delete(server: *mut Server) -> u32;
    pub(crate) fn UA_Server_getLifecycleState(server: *mut Server) -> Lifecycle;
    pub(crate) fn shim_response_result(response: *const c_void) -> u32;
    pub(crate) fn shim_server_discovery_url(
        server: *mut Server,
        index: usize,
    ) -> *const Bytes;
}
