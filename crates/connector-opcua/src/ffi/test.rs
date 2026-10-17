use std::ffi::{c_int, c_void};
use std::mem::offset_of;

use super::{
    Bytes, ConnectionCallback, ConnectionManager, EventLoop, KeyValueMap, Node, at,
};

/// `UA_Server`, which Rust holds only by pointer.
#[repr(C)]
pub(crate) struct Server([u8; 0]);

/// `shim_counts` of `server.c`.
#[repr(C)]
pub(crate) struct Counts {
    pub(crate) browses: usize,
    pub(crate) points: usize,
}

/// `UA_LifecycleState` of a server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct Lifecycle(pub(crate) c_int);

impl Lifecycle {
    pub(crate) const STOPPED: Self = Self(0);
    pub(crate) const STARTED: Self = Self(1);
    pub(crate) const STOPPING: Self = Self(2);
}

/// `UA_NS0ID_SERVER_SERVERSTATUS_CURRENTTIME`, the node of the current time of a
/// server. `shim.c` asserts it.
pub(crate) const TIME: u32 = 2258;

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

/// `UA_NODEIDTYPE_NUMERIC`.
pub(crate) const NUMERIC: c_int = 0;

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

/// A constructor of an RSA policy.
pub(crate) type Rsa =
    unsafe extern "C" fn(*mut c_void, Bytes, Bytes, *const c_void) -> u32;

/// A constructor of an ECC policy, which takes the type of the application too.
pub(crate) type Ecc =
    unsafe extern "C" fn(*mut c_void, c_int, Bytes, Bytes, *const c_void) -> u32;

unsafe extern "C" {
    pub(crate) fn UA_readNumber(
        buf: *const u8,
        length: usize,
        number: *mut u32,
    ) -> usize;
    pub(crate) fn UA_UInt32_random() -> u32;
    pub(crate) fn UA_DateTime_now() -> i64;
    pub(crate) fn UA_DateTime_nowMonotonic() -> i64;
    pub(crate) fn UA_DateTime_localTimeUtcOffset() -> i64;

    pub(crate) fn UA_EventLoop_new_POSIX(logger: *const c_void) -> *mut c_void;
    pub(crate) fn UA_ConnectionManager_new_POSIX_TCP(name: Bytes) -> *mut c_void;
    pub(crate) fn UA_ConnectionManager_new_POSIX_UDP(name: Bytes) -> *mut c_void;
    pub(crate) fn UA_ConnectionManager_new_POSIX_Ethernet(name: Bytes) -> *mut c_void;
    pub(crate) fn UA_InterruptManager_new_POSIX(name: Bytes) -> *mut c_void;

    pub(crate) fn UA_SecurityPolicy_Basic128Rsa15(
        policy: *mut c_void,
        certificate: Bytes,
        key: Bytes,
        logger: *const c_void,
    ) -> u32;
    pub(crate) fn UA_SecurityPolicy_Basic256(
        policy: *mut c_void,
        certificate: Bytes,
        key: Bytes,
        logger: *const c_void,
    ) -> u32;
    pub(crate) fn UA_SecurityPolicy_EccBrainpoolP256r1(
        policy: *mut c_void,
        kind: c_int,
        certificate: Bytes,
        key: Bytes,
        logger: *const c_void,
    ) -> u32;
    pub(crate) fn UA_SecurityPolicy_EccBrainpoolP384r1(
        policy: *mut c_void,
        kind: c_int,
        certificate: Bytes,
        key: Bytes,
        logger: *const c_void,
    ) -> u32;
    pub(crate) fn UA_SecurityPolicy_EccCurve25519(
        policy: *mut c_void,
        kind: c_int,
        certificate: Bytes,
        key: Bytes,
        logger: *const c_void,
    ) -> u32;
    pub(crate) fn UA_SecurityPolicy_EccCurve448(
        policy: *mut c_void,
        kind: c_int,
        certificate: Bytes,
        key: Bytes,
        logger: *const c_void,
    ) -> u32;
    pub(crate) fn UA_SecurityPolicy_EccNistP256(
        policy: *mut c_void,
        kind: c_int,
        certificate: Bytes,
        key: Bytes,
        logger: *const c_void,
    ) -> u32;
    pub(crate) fn UA_SecurityPolicy_EccNistP256AesGcm(
        policy: *mut c_void,
        kind: c_int,
        certificate: Bytes,
        key: Bytes,
        logger: *const c_void,
    ) -> u32;
    pub(crate) fn UA_SecurityPolicy_EccNistP256ChaChaPoly(
        policy: *mut c_void,
        kind: c_int,
        certificate: Bytes,
        key: Bytes,
        logger: *const c_void,
    ) -> u32;
    pub(crate) fn UA_SecurityPolicy_EccNistP384(
        policy: *mut c_void,
        kind: c_int,
        certificate: Bytes,
        key: Bytes,
        logger: *const c_void,
    ) -> u32;
    #[cfg(any(target_os = "linux", windows))]
    pub(crate) fn UA_CertificateGroup_Filestore(
        group: *mut c_void,
        key: *mut c_void,
        path: Bytes,
        logger: *const c_void,
        params: *mut c_void,
    ) -> u32;
    #[cfg(any(target_os = "linux", windows))]
    pub(crate) fn UA_SecurityPolicy_Filestore(
        policy: *mut c_void,
        inner: *mut c_void,
        path: Bytes,
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

    pub(crate) fn shim_server_new(
        el: *mut EventLoop,
        port: u16,
        host: *const std::ffi::c_char,
        sessions: u16,
        chunks: u32,
    ) -> *mut Server;
    pub(crate) fn UA_Server_run_startup(server: *mut Server) -> u32;
    pub(crate) fn shim_server_limits(
        server: *mut Server,
        buffer: u32,
        message: u32,
        chunks: u32,
    );
    pub(crate) fn shim_client_receive(client: *mut super::Client, size: u32);
    pub(crate) fn shim_server_reads(server: *mut Server, most: u32);
    pub(crate) fn shim_server_references(server: *mut Server, most: u32);
    pub(crate) fn shim_server_browses(server: *mut Server, browse: u32, translate: u32);
    pub(crate) fn shim_server_count(server: *mut Server) -> *const Counts;
    pub(crate) fn UA_Client_getConnectionAttribute(
        client: *mut super::Client,
        key: QualifiedName,
        value: *mut c_void,
    ) -> u32;
    pub(crate) fn shim_server_add_source(
        server: *mut Server,
        node: Node<'_>,
        source: *mut c_void,
    ) -> u32;
    pub(crate) fn shim_server_set_source(
        server: *mut Server,
        node: Node<'_>,
        source: *mut c_void,
    ) -> u32;
    pub(crate) fn shim_server_session_timeout(server: *mut Server, ms: u32);
    pub(crate) fn shim_server_data_type(
        server: *mut Server,
        node: Node<'_>,
        id: *mut u32,
    ) -> u32;
    pub(crate) fn shim_server_add_folder(
        server: *mut Server,
        node: Node<'_>,
        parent: Node<'_>,
        reference: u32,
        name: *const std::ffi::c_char,
        display: *const std::ffi::c_char,
    ) -> u32;
    pub(crate) fn shim_server_add_guid(server: *mut Server, parent: Node<'_>) -> u32;
    pub(crate) fn shim_server_add_variable(
        server: *mut Server,
        node: Node<'_>,
        parent: Node<'_>,
        reference: u32,
        name: *const std::ffi::c_char,
        data_type: Node<'_>,
        rank: i32,
        access: u8,
        encoded: *const u8,
        size: usize,
    ) -> u32;
    pub(crate) fn shim_server_add_data_type(
        server: *mut Server,
        node: Node<'_>,
        parent: Node<'_>,
        name: *const std::ffi::c_char,
    ) -> u32;
    pub(crate) fn shim_server_add_unit(
        server: *mut Server,
        node: Node<'_>,
        variable: Node<'_>,
        uri: *const std::ffi::c_char,
        text: *const std::ffi::c_char,
        code: i32,
    ) -> u32;
    pub(crate) fn shim_server_add_reference(
        server: *mut Server,
        source: Node<'_>,
        reference: u32,
        target: Node<'_>,
    ) -> u32;
    pub(crate) fn shim_server_delete_reference(
        server: *mut Server,
        source: Node<'_>,
        reference: u32,
        target: Node<'_>,
    ) -> u32;
    pub(crate) fn shim_server_add_view(
        server: *mut Server,
        node: Node<'_>,
        parent: Node<'_>,
        name: *const std::ffi::c_char,
    ) -> u32;
    pub(crate) fn UA_Server_run_shutdown(server: *mut Server) -> u32;
    pub(crate) fn UA_Server_delete(server: *mut Server) -> u32;
    pub(crate) fn UA_Server_getLifecycleState(server: *mut Server) -> Lifecycle;
    pub(crate) fn UA_Server_addNamespace(
        server: *mut Server,
        name: *const std::ffi::c_char,
    ) -> u16;
    pub(crate) fn shim_value_status(value: *const super::DataValue) -> u32;
    pub(crate) fn shim_server_discovery_url(
        server: *mut Server,
        index: usize,
    ) -> *const Bytes;
}
