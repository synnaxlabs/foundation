//! The test server of open62541, on the loop of a connection manager.

#![expect(unsafe_code, reason = "open62541 is a C library")]

#[cfg(feature = "sim")]
use std::cell::RefCell;
use std::ffi::{CStr, c_int};
use std::future::poll_fn;
use std::mem::ManuallyDrop;
use std::panic::{self, AssertUnwindSafe};
use std::pin::pin;
use std::ptr::NonNull;
use std::task::Poll;
use std::thread;

use env::net;

use crate::connection::Manager;

use crate::event;
use crate::ffi::{self, Status};
use crate::node::{Key, Node, ns0};
use crate::value::Value;
#[cfg(test)]
use types::time::Span;
#[cfg(feature = "sim")]
use types::time::Stamp;

/// The channel security of a test server of [`Server::start_secure`].
pub(crate) struct Secure<'a> {
    /// The certificate and key of the server, and the only certificates that its
    /// channels trust.
    pub(crate) credentials: &'a crate::client::Credentials<'a>,
    /// The policies of its endpoints, in order.
    pub(crate) policies: &'a [crate::secure::Policy],
    /// The modes of an endpoint of each policy, in order.
    pub(crate) modes: &'a [crate::client::Mode],
}

/// A started test server of open62541 on the loop of a connection manager.
/// [`Server::close`] deletes it, and a drop leaks it.
pub(crate) struct Server {
    raw: NonNull<ffi::test::Server>,
    /// The loop that the server lives on.
    events: *mut ffi::EventLoop,
    /// The context of each variable of [`Server::add_source`], until the delete.
    #[cfg(feature = "sim")]
    sources: RefCell<Vec<NonNull<Source>>>,
}

/// The node context of a variable of [`Server::add_source`]: `shim_source` of
/// `server.c`, then the read.
#[cfg(feature = "sim")]
#[repr(C)]
struct Source {
    read: unsafe extern "C" fn(
        *mut Source,
        *mut ffi::Scalar,
        *mut u16,
        *mut u32,
        *mut i64,
    ),
    answer: Box<dyn FnMut() -> Answer>,
    /// False while the add reads the variable to check its type.
    added: bool,
}

/// The answer of a read of a variable of [`Server::add_source`]: the value, its
/// status, and its source timestamp, or `None` for the time of the read.
#[cfg(feature = "sim")]
pub(crate) type Answer = (Value, types::quality::Quality, Option<Stamp>);

/// Answers a read of the variable of `source` into `value`, `kind`, `status`, and
/// `ticks`, the source timestamp as a `DateTime`, or 0 for none.
///
/// # Safety
///
/// `source` is live, nothing else holds it, and `value`, `kind`, `status`, and
/// `ticks` are places to write.
#[cfg(feature = "sim")]
unsafe extern "C" fn read(
    source: *mut Source,
    value: *mut ffi::Scalar,
    kind: *mut u16,
    status: *mut u32,
    ticks: *mut i64,
) {
    // SAFETY: as the caller ensures.
    let source = unsafe { &mut *source };
    let (answer, quality, stamp) = if source.added {
        (source.answer)()
    } else {
        (Value::F64(0.0), types::quality::Quality::GOOD, None)
    };
    let (scalar, scalar_kind) = answer.raw();
    // SAFETY: as the caller ensures.
    unsafe { value.write(scalar) };
    // SAFETY: as the caller ensures.
    unsafe { kind.write(scalar_kind) };
    // SAFETY: as the caller ensures.
    unsafe { status.write(quality.0) };
    let time = stamp.map_or(0, |stamp| stamp.nanos().div_euclid(100) + ffi::UNIX_EPOCH);
    // SAFETY: as the caller ensures.
    unsafe { ticks.write(time) };
}

impl Server {
    /// Makes the test server on the loop of `manager`, with the minimal config, the one
    /// server URL `opc.tcp://{host}:{port}` on the port of the listener of `manager`,
    /// and room for `sessions` sessions and secure channels, and starts it. With an
    /// empty `host`, the discovery URL of the server has the address of the listener as
    /// its host, and a listener on an unspecified address gives the server no discovery
    /// URL.
    ///
    /// # Safety
    ///
    /// The server is deleted before `manager` drops: by [`Server::close`], or by a
    /// delete through [`Server::raw`].
    ///
    /// # Panics
    ///
    /// If `manager` has no listener that a listen open did not take, or open62541
    /// refuses the server or its start.
    pub(crate) unsafe fn start(manager: &Manager, host: &CStr, sessions: u16) -> Self {
        // SAFETY: as the caller ensures.
        unsafe { Self::started(manager, host, sessions, std::ptr::null(), &[], &[]) }
    }

    /// Makes and starts the test server as [`Server::start`] does, which has no
    /// endpoint in `None`, but an endpoint in each mode for each policy of `secure`,
    /// in that order, on its credentials.
    ///
    /// # Safety
    ///
    /// As [`Server::start`].
    ///
    /// # Panics
    ///
    /// As [`Server::start`].
    pub(crate) unsafe fn start_secure(
        manager: &Manager,
        host: &CStr,
        sessions: u16,
        secure: &Secure<'_>,
    ) -> Self {
        let raw = secure.credentials.raw();
        let uris: Vec<_> = (secure.policies.iter())
            .map(|policy| policy.uri().as_ptr())
            .collect();
        let modes: Vec<_> = (secure.modes.iter())
            .map(|&mode| ffi::Mode::from(mode))
            .collect();
        // SAFETY: as the caller ensures, and the server copies `raw`, `uris`, and
        // `modes`.
        unsafe { Self::started(manager, host, sessions, &raw const raw, &uris, &modes) }
    }

    /// [`Server::start`], with the policies of `security` and an endpoint in each of
    /// `modes` for each of the policy URIs `policies`, in place of the endpoint in
    /// `None`, when `security` is not null.
    ///
    /// # Safety
    ///
    /// As [`Server::start`], `security` is null or points at a live `shim_security`,
    /// and each of `policies` is a live C string.
    unsafe fn started(
        manager: &Manager,
        host: &CStr,
        sessions: u16,
        security: *const ffi::Security<'_>,
        policies: &[*const std::ffi::c_char],
        modes: &[ffi::Mode],
    ) -> Self {
        let port = manager
            .port()
            .expect("a server starts on a manager with a listener that no server took");
        let events = manager.events().raw();
        // SAFETY: the loop outlives the server, as the caller ensures, and `security`
        // is null or live.
        let raw = unsafe {
            ffi::test::shim_server_new(
                events,
                port,
                host.as_ptr(),
                sessions,
                crate::connection::CHUNKS,
                security,
                policies.as_ptr(),
                policies.len(),
                modes.as_ptr(),
                modes.len(),
            )
        };
        let raw = NonNull::new(raw).expect("open62541 refused the server");
        // SAFETY: the server lives.
        let status = Status(unsafe { ffi::test::UA_Server_run_startup(raw.as_ptr()) });
        assert_eq!(status, Status::GOOD, "open62541 refused the server start");
        Self {
            raw,
            events,
            #[cfg(feature = "sim")]
            sources: RefCell::default(),
        }
    }

    /// Adds the namespace `uri`, and gives its index.
    #[cfg(any(test, feature = "sim"))]
    pub(crate) fn add_namespace(&self, uri: &CStr) -> u16 {
        // SAFETY: the server lives, and it copies the name.
        unsafe { ffi::test::UA_Server_addNamespace(self.raw(), uri.as_ptr()) }
    }

    /// Adds the variable `node` with `value`, of its type, under the Objects folder.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the node, or the key of `node` is the number 0.
    pub(crate) fn add_scalar(&self, node: &Node, value: Value) {
        let (id, value) = variant(value);
        self.add_variable(&objects(node, c"value", id, value));
    }

    /// Adds `variable`.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the node or the decode of its value, or the key of its node
    /// is the number 0.
    pub(crate) fn add_variable(&self, variable: &Variable<'_>) {
        let (value, size) = variable
            .value
            .as_ref()
            .map_or((std::ptr::null(), 0), |value| (value.as_ptr(), value.len()));
        // SAFETY: the server lives, and it copies each name and the decoded value.
        let status = Status(unsafe {
            ffi::test::shim_server_add_variable(
                self.raw(),
                added(&variable.node),
                (&variable.parent).into(),
                variable.reference,
                variable.name.as_ptr(),
                (&variable.data_type).into(),
                variable.rank,
                variable.access,
                value,
                size,
            )
        });
        assert_eq!(status, Status::GOOD, "open62541 refused the node");
    }

    /// Adds the variable `node` with an array of two Doubles, under the Objects
    /// folder.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the node, or the key of `node` is the number 0.
    #[cfg(test)]
    pub(crate) fn add_doubles(&self, node: &Node) {
        self.add_variable(&Variable {
            rank: 1,
            ..objects(node, c"doubles", ns0::DOUBLE, doubles())
        });
    }

    /// Adds the scalar variable `node` of the `DataType` `BaseDataType`, and answers
    /// each read of a client with the value, the status, and the source timestamp that
    /// `answer` gives, on the thread that drives the server.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the node, or the key of `node` is the number 0.
    #[cfg(feature = "sim")]
    pub(crate) fn add_source(
        &self,
        node: &Node,
        answer: impl FnMut() -> Answer + 'static,
    ) {
        let source = self.source(Box::new(answer), false);
        // SAFETY: the server lives, it copies the name of `node`, and the close frees
        // `source` after the delete.
        let status = Status(unsafe {
            ffi::test::shim_server_add_source(
                self.raw(),
                added(node),
                source.as_ptr().cast(),
            )
        });
        assert_eq!(status, Status::GOOD, "open62541 refused the node");
        // SAFETY: it lives, and the server reads it only while the loop runs.
        unsafe { (*source.as_ptr()).added = true };
    }

    /// Caps each page of a Browse of the server at `most` references.
    #[cfg(feature = "sim")]
    pub(crate) fn cap_references(&self, most: u32) {
        // SAFETY: the server lives.
        unsafe { ffi::test::shim_server_references(self.raw(), most) };
    }

    /// Makes `answer` answer each later read of the variable `node`, which open62541
    /// adds, such as the state of the server, as [`Server::add_source`] does, with the
    /// time of the read as the source timestamp.
    ///
    /// # Panics
    ///
    /// If open62541 has no variable `node`, or its key is the number 0.
    #[cfg(test)]
    pub(crate) fn set_source(
        &self,
        node: &Node,
        mut answer: impl FnMut() -> (Value, types::quality::Quality) + 'static,
    ) {
        let answer = move || {
            let (value, quality) = answer();
            (value, quality, None)
        };
        let source = self.source(Box::new(answer), true);
        // SAFETY: the server lives, and the close frees `source` after the delete.
        let status = Status(unsafe {
            ffi::test::shim_server_set_source(
                self.raw(),
                added(node),
                source.as_ptr().cast(),
            )
        });
        assert_eq!(status, Status::GOOD, "open62541 refused the source");
    }

    /// Makes a [`Source`] of `answer` that the close frees.
    #[cfg(feature = "sim")]
    fn source(
        &self,
        answer: Box<dyn FnMut() -> Answer>,
        added: bool,
    ) -> NonNull<Source> {
        let source = NonNull::from(Box::leak(Box::new(Source {
            read,
            answer,
            added,
        })));
        self.sources.borrow_mut().push(source);
        source
    }

    /// Sets the most session timeout that the server gives a later session, in whole
    /// ms.
    ///
    /// # Panics
    ///
    /// If `most` is past `u32::MAX` ms.
    #[cfg(test)]
    pub(crate) fn set_session_timeout(&self, most: Span) {
        let ms = u32::try_from(most.nanos() / 1_000_000).expect("a timeout of u32 ms");
        // SAFETY: the server lives.
        unsafe { ffi::test::shim_server_session_timeout(self.raw(), ms) };
    }

    /// Lets in a later session only with the user name and password of one of
    /// `users`, and no anonymous session.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the logins.
    #[cfg(test)]
    pub(crate) fn admit(&self, users: &[(&str, &[u8])]) {
        let logins: Vec<_> = (users.iter())
            .map(|(name, password)| ffi::test::Login {
                name: ffi::Bytes::of(name.as_bytes()),
                password: ffi::Bytes::of(password),
            })
            .collect();
        // SAFETY: the server lives, and it copies `logins`.
        let status = Status(unsafe {
            ffi::test::shim_server_users(self.raw(), logins.as_ptr(), logins.len())
        });
        assert_eq!(status, Status::GOOD, "open62541 refused the logins");
    }

    /// Gives each endpoint one user token policy, a user name with the policy `None`,
    /// so a client that sends the password sends it in clear on a channel that does
    /// not encrypt.
    ///
    /// # Panics
    ///
    /// If memory runs out.
    #[cfg(test)]
    pub(crate) fn cleartext(&self) {
        // SAFETY: the server lives.
        let status = Status(unsafe { ffi::test::shim_server_cleartext(self.raw()) });
        assert_eq!(status, Status::GOOD, "open62541 refused the token policies");
    }

    /// The number of the `DataType` of the variable `node`, in namespace 0.
    ///
    /// # Panics
    ///
    /// If the server has no variable `node`, or its `DataType` is not a number of
    /// namespace 0.
    #[cfg(test)]
    pub(crate) fn data_type(&self, node: &Node) -> u32 {
        let mut id = 0;
        // SAFETY: the server lives, and `id` is a place to write.
        let status = Status(unsafe {
            ffi::test::shim_server_data_type(self.raw(), node.into(), &raw mut id)
        });
        assert_eq!(status, Status::GOOD, "{node}: open62541 gives no DataType");
        id
    }

    /// Adds the variable `node` with the String `text`, under the Objects folder.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the node, or the key of `node` is the number 0.
    #[cfg(test)]
    pub(crate) fn add_string(&self, node: &Node) {
        let text = [&4_i32.to_le_bytes()[..], b"text"].concat();
        let value = scalar(ns0::STRING, &text);
        self.add_variable(&objects(node, c"string", ns0::STRING, value));
    }

    /// Adds the variable `node` with a `ByteString` of `size` zero bytes, under the
    /// Objects folder.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the node, `size` is past `i32::MAX`, or the key of `node`
    /// is the number 0.
    pub(crate) fn add_bytes(&self, node: &Node, size: usize) {
        let length =
            i32::try_from(size).expect("a ByteString of at most i32::MAX bytes");
        let mut bytes = length.to_le_bytes().to_vec();
        bytes.resize(4 + size, 0);
        let value = scalar(ns0::BYTE_STRING, &bytes);
        self.add_variable(&objects(node, c"bytes", ns0::BYTE_STRING, value));
    }

    /// Refuses each Read of more than `most` nodes, 0 for none, with
    /// `BadTooManyOperations`. It adds no variable `ns=0;i=11705`, so a client does
    /// not know the limit.
    pub(crate) fn refuse_reads_past(&self, most: u32) {
        // SAFETY: the server lives.
        unsafe { ffi::test::shim_server_reads(self.raw(), most) };
    }

    /// Sets the limits of each later channel of the server.
    #[cfg(test)]
    pub(crate) fn set_limits(&self, limits: Limits) {
        // SAFETY: the server lives.
        unsafe {
            ffi::test::shim_server_limits(
                self.raw(),
                limits.buffer,
                limits.message,
                limits.chunks,
            );
        };
    }

    /// Sets the `MaxNodesPerRead` of the server to `most`, 0 for none, and adds the
    /// variable `ns=0;i=11705` that gives it, which the build of open62541 lacks.
    ///
    /// # Panics
    ///
    /// On a second call, as open62541 refuses the second add of the variable.
    pub(crate) fn set_per_read(&self, most: u32) {
        self.refuse_reads_past(most);
        self.add_scalar(&Node::ns0(ffi::MAX_NODES_PER_READ), Value::U32(most));
    }

    /// Sets the `MaxNodesPerBrowse` of the server to `browse` and its
    /// `MaxNodesPerTranslateBrowsePathsToNodeIds` to `translate`, each 0 for none, and
    /// adds the variables `ns=0;i=11710` and `ns=0;i=11712` that give them.
    ///
    /// # Panics
    ///
    /// On a second call, as open62541 refuses the second add of each variable.
    #[cfg(test)]
    pub(crate) fn set_per_browse(&self, browse: u32, translate: u32) {
        // SAFETY: the server lives.
        unsafe { ffi::test::shim_server_browses(self.raw(), browse, translate) };
        for (number, most) in [(11710, browse), (11712, translate)] {
            self.add_scalar(&Node::ns0(number), Value::U32(most));
        }
    }

    /// Refuses each `CreateMonitoredItems` of more than `most` items, 0 for none, with
    /// `BadTooManyOperations`. It adds no variable `ns=0;i=11714`, so a client does
    /// not know the limit.
    #[cfg(test)]
    pub(crate) fn refuse_monitors_past(&self, most: u32) {
        // SAFETY: the server lives.
        unsafe { ffi::test::shim_server_monitors(self.raw(), most) };
    }

    /// Sets the `MaxMonitoredItemsPerCall` of the server to `most`, 0 for none, and
    /// adds the variable `ns=0;i=11714` that gives it, which the build of open62541
    /// lacks.
    ///
    /// # Panics
    ///
    /// On a second call, as open62541 refuses the second add of the variable.
    #[cfg(test)]
    pub(crate) fn set_per_monitor(&self, most: u32) {
        self.refuse_monitors_past(most);
        let node = Node::ns0(ffi::MAX_MONITORED_ITEMS_PER_CALL);
        self.add_scalar(&node, Value::U32(most));
    }

    /// Ends each subscription of the server with `status`, which the server sends in
    /// a status change notification at the next publishing interval of each.
    #[cfg(test)]
    pub(crate) fn end_subscriptions(&self, status: Status) {
        // SAFETY: the server lives.
        unsafe { ffi::test::shim_server_end(self.raw(), status.0) };
    }

    /// Sets the most notifications of a Publish answer that the server grants.
    #[cfg(feature = "sim")]
    pub(crate) fn set_per_publish(&self, most: u32) {
        // SAFETY: the server lives.
        unsafe { ffi::test::shim_server_publishes(self.raw(), most) };
    }

    /// Sets the shortest publishing interval that the server grants, in ms.
    #[cfg(test)]
    pub(crate) fn set_publishing(&self, ms: f64) {
        // SAFETY: the server lives.
        unsafe { ffi::test::shim_server_publishing(self.raw(), ms) };
    }

    /// Sets the smallest keep-alive count of a subscription that the server grants.
    #[cfg(test)]
    pub(crate) fn set_keep_alives(&self, least: u32) {
        // SAFETY: the server lives.
        unsafe { ffi::test::shim_server_keep_alives(self.raw(), least) };
    }

    /// Gives the most Publish requests that one session of the server holds now.
    #[cfg(test)]
    pub(crate) fn held(&self) -> u32 {
        // SAFETY: the server lives.
        unsafe { ffi::test::shim_server_held(self.raw()) }
    }

    /// Sets the most Publish requests that the server holds for each session. It
    /// answers the oldest with `BadTooManyPublishRequests` to take one more.
    #[cfg(test)]
    pub(crate) fn set_held(&self, most: u32) {
        // SAFETY: the server lives.
        unsafe { ffi::test::shim_server_holds(self.raw(), most) };
    }

    /// Sets the largest queue of a monitored item that the server grants.
    #[cfg(test)]
    pub(crate) fn set_queues(&self, most: u32) {
        // SAFETY: the server lives.
        unsafe { ffi::test::shim_server_queues(self.raw(), most) };
    }

    /// Gives the most notifications of a Publish answer of the first subscription of
    /// the server, 0 for none.
    #[cfg(test)]
    pub(crate) fn per_publish(&self) -> u32 {
        // SAFETY: the server lives.
        unsafe { ffi::test::shim_server_per_publish(self.raw()) }
    }

    /// Counts each Browse, `CreateMonitoredItems`, and Publish request that the server
    /// processes from now, and the most continuation points that one session holds.
    ///
    /// # Panics
    ///
    /// If open62541 cannot allocate the counts.
    #[cfg(test)]
    pub(crate) fn count(&self) -> Counts<'_> {
        // SAFETY: the server lives.
        let counts = unsafe { ffi::test::shim_server_count(self.raw()) };
        Counts {
            counts: NonNull::new(counts.cast_mut())
                .expect("open62541 allocates the counts"),
            server: std::marker::PhantomData,
        }
    }

    /// Adds the variable `ns=0;i=2735` that gives `most` as the
    /// `MaxBrowseContinuationPoints` of the server. The server holds 32 points for each
    /// session, whatever it gives.
    ///
    /// # Panics
    ///
    /// On a second call, as open62541 refuses the second add of the variable.
    #[cfg(test)]
    pub(crate) fn report_points(&self, most: u16) {
        self.add_scalar(&Node::ns0(2735), Value::U16(most));
    }

    /// Adds the folder `node` with the browse name `name` and the display name
    /// `display`, under `parent` by the reference type of the number `reference` in
    /// namespace 0.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the node, or the key of `node` is the number 0.
    pub(crate) fn add_folder(
        &self,
        node: &Node,
        parent: &Node,
        reference: u32,
        name: &CStr,
        display: &CStr,
    ) {
        // SAFETY: the server lives, and it copies each name.
        let status = Status(unsafe {
            ffi::test::shim_server_add_folder(
                self.raw(),
                added(node),
                parent.into(),
                reference,
                name.as_ptr(),
                display.as_ptr(),
            )
        });
        assert_eq!(status, Status::GOOD, "open62541 refused the node");
    }

    /// Adds the folder of a GUID in namespace 1 with the browse and display name
    /// `guid`, under `parent` by `Organizes`.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the node.
    pub(crate) fn add_guid(&self, parent: &Node) {
        // SAFETY: the server lives, and it copies the name of `parent`.
        let status = Status(unsafe {
            ffi::test::shim_server_add_guid(self.raw(), parent.into())
        });
        assert_eq!(status, Status::GOOD, "open62541 refused the node");
    }

    /// Adds the data type `node` with the browse name `name`, a subtype of `parent`.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the node, or the key of `node` is the number 0.
    pub(crate) fn add_data_type(&self, node: &Node, parent: &Node, name: &CStr) {
        // SAFETY: the server lives, and it copies each name.
        let status = Status(unsafe {
            ffi::test::shim_server_add_data_type(
                self.raw(),
                added(node),
                parent.into(),
                name.as_ptr(),
            )
        });
        assert_eq!(status, Status::GOOD, "open62541 refused the node");
    }

    /// Adds the property `0:EngineeringUnits` `node` of `variable`, an
    /// `EUInformation` of the `namespaceUri` `uri`, the display name `text`, and the
    /// `unitId` `code`.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the node, or the key of `node` is the number 0.
    pub(crate) fn add_unit(
        &self,
        node: &Node,
        variable: &Node,
        uri: &CStr,
        text: &CStr,
        code: i32,
    ) {
        // SAFETY: the server lives, and it copies each name and text.
        let status = Status(unsafe {
            ffi::test::shim_server_add_unit(
                self.raw(),
                added(node),
                variable.into(),
                uri.as_ptr(),
                text.as_ptr(),
                code,
            )
        });
        assert_eq!(status, Status::GOOD, "open62541 refused the node");
    }

    /// Adds a reference of the type of the number `reference` in namespace 0 from
    /// `source` to `target`.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the reference.
    pub(crate) fn add_reference(&self, source: &Node, reference: u32, target: &Node) {
        // SAFETY: the server lives, and it copies each name.
        let status = Status(unsafe {
            ffi::test::shim_server_add_reference(
                self.raw(),
                source.into(),
                reference,
                target.into(),
            )
        });
        assert_eq!(status, Status::GOOD, "open62541 refused the reference");
    }

    /// Adds the view `node` with the browse and display name `name`, under `parent` by
    /// `Organizes`.
    ///
    /// # Panics
    ///
    /// If open62541 refuses the node, or the key of `node` is the number 0.
    #[cfg(test)]
    pub(crate) fn add_view(&self, node: &Node, parent: &Node, name: &CStr) {
        // SAFETY: the server lives, and it copies each name.
        let status = Status(unsafe {
            ffi::test::shim_server_add_view(
                self.raw(),
                added(node),
                parent.into(),
                name.as_ptr(),
            )
        });
        assert_eq!(status, Status::GOOD, "open62541 refused the node");
    }

    /// Deletes the reference of the type of the number `reference` in namespace 0
    /// from `source` to `target`, in both directions.
    ///
    /// # Panics
    ///
    /// If the server has no such reference.
    #[cfg(test)]
    pub(crate) fn delete_reference(
        &self,
        source: &Node,
        reference: u32,
        target: &Node,
    ) {
        // SAFETY: the server lives, and it copies each name.
        let status = Status(unsafe {
            ffi::test::shim_server_delete_reference(
                self.raw(),
                source.into(),
                reference,
                target.into(),
            )
        });
        assert_eq!(status, Status::GOOD, "open62541 refused the delete");
    }

    /// Gives the server, for a call that reads or changes it.
    pub(crate) fn raw(&self) -> *mut ffi::test::Server {
        self.raw.as_ptr()
    }

    /// Starts a server on `manager` with an empty host and room for `sessions`
    /// sessions, as [`Server::start`] does, or as [`Server::start_secure`] does on
    /// `secure`, and gives it to `add`. Then
    /// drives `manager` until `until` completes or the accept of its listener fails,
    /// closes the server as [`Server::close`] does, drops `manager`, and gives the
    /// output of `until`.
    ///
    /// # Errors
    ///
    /// The error of the accept, when it fails before `until` completes. A failure in
    /// the same poll as the end of `until` wins.
    ///
    /// # Panics
    ///
    /// As [`Server::start`] and [`Server::close`] do. A panic of `add` closes the
    /// server and drops `manager` before it goes on. Another panic leaks the server and
    /// `manager`. A drop of the future while it waits, as in a crash of its node,
    /// closes the server and drops `manager` in the drop, with no wait.
    pub(crate) async fn serve<T>(
        manager: Manager,
        sessions: u16,
        secure: Option<&Secure<'_>>,
        add: impl FnOnce(&mut Self),
        until: impl Future<Output = T>,
    ) -> Result<T, net::Error> {
        let manager = ManuallyDrop::new(manager);
        let server = match secure {
            // SAFETY: each close deletes the server before the manager drops. A panic
            // out of the start leaks the manager, and the server if open62541 made it.
            Some(secure) => unsafe {
                Self::start_secure(&manager, c"", sessions, secure)
            },
            // SAFETY: as above.
            None => unsafe { Self::start(&manager, c"", sessions) },
        };
        let mut serving = Serving {
            server: Some(server),
            manager,
        };
        let added = panic::catch_unwind(AssertUnwindSafe(|| {
            add(serving.server.as_mut().expect("invariant: not closed yet"));
        }));
        if let Err(panic) = added {
            serving.close().await;
            drop(serving);
            panic::resume_unwind(panic);
        }
        let manager = &serving.manager;
        let out = {
            let mut drive = pin!(manager.drive(|_| match manager.accept_error() {
                Some(e) => Poll::Ready(e),
                None => Poll::Pending,
            }));
            let mut until = pin!(until);
            poll_fn(|cx| {
                if let Poll::Ready(e) = drive.as_mut().poll(cx) {
                    return Poll::Ready(Err(e));
                }
                until.as_mut().poll(cx).map(Ok)
            })
            .await
        };
        serving.close().await;
        out
    }

    /// Stops the server, drives `manager` until no delayed callback waits on its loop,
    /// then deletes the server. It does not wait for a timer.
    ///
    /// # Panics
    ///
    /// If `manager` does not hold the loop of the server. If open62541 refuses the stop
    /// or the delete, or fails a run of the loop. If a delayed callback still waits
    /// after 64 runs of the loop: a loop whose delayed callbacks do not drain is a
    /// defect. Each panic but the refused delete comes before the delete, so the
    /// server still lives after it.
    pub(crate) async fn close(self, manager: &Manager) {
        let events = self.stop(manager);
        let mut runs = 0;
        manager.drive(|_| drained(events, &mut runs)).await;
        self.delete();
    }

    /// Closes the server as [`Server::close`] does, but only runs the loop: no
    /// connection reads, writes, or sleeps, so it can run in a drop where no I/O can.
    /// Each connection closes with what waits on it unsent.
    ///
    /// # Panics
    ///
    /// As [`Server::close`].
    fn close_now(self, manager: &Manager) {
        let events = self.stop(manager);
        let mut runs = 0;
        loop {
            events.run();
            if drained(events, &mut runs).is_ready() {
                break;
            }
        }
        self.delete();
    }

    /// Stops the server, and gives the loop of `manager`.
    fn stop<'m>(&self, manager: &'m Manager) -> &'m event::Loop {
        let events = manager.events();
        assert!(
            events.raw() == self.events,
            "a server closes through the manager it started on"
        );
        // SAFETY: the server lives.
        let status = Status(unsafe { ffi::test::UA_Server_run_shutdown(self.raw()) });
        assert_eq!(status, Status::GOOD, "open62541 refused the server stop");
        events
    }

    /// Deletes the stopped server, and frees the context of each source.
    fn delete(self) {
        // SAFETY: the server lives.
        let state = unsafe { ffi::test::UA_Server_getLifecycleState(self.raw()) };
        assert_eq!(
            state,
            ffi::test::Lifecycle::STOPPED,
            "invariant: a close queues its `CLOSING` at once"
        );
        // SAFETY: the server is stopped, and nothing holds it.
        let status = Status(unsafe { ffi::test::UA_Server_delete(self.raw()) });
        assert_eq!(status, Status::GOOD, "open62541 refused the server delete");
        #[cfg(feature = "sim")]
        for source in self.sources.into_inner() {
            // SAFETY: the server that read it is deleted, and nothing else holds it.
            drop(unsafe { Box::from_raw(source.as_ptr()) });
        }
    }
}

/// Counts a run of the loop of a close in `runs`, and gives `Ready` once no delayed
/// callback waits on `events`.
///
/// # Panics
///
/// If one still waits after run 64.
fn drained(events: &event::Loop, runs: &mut c_int) -> Poll<()> {
    *runs += 1;
    if !events.delayed() {
        return Poll::Ready(());
    }
    // SAFETY: a constant of `shim.c`.
    let passes = unsafe { ffi::shim_free_passes };
    assert!(
        *runs < passes,
        "open62541 queued delayed callbacks for {passes} runs of a server close"
    );
    Poll::Pending
}

/// A server of [`Server::serve`] and its manager. A drop that is not in a panic
/// closes the server, if it still lives, as [`Server::close_now`] does, and drops the
/// manager. A drop in a panic leaks both, as the server may be in a call that
/// panicked.
struct Serving {
    server: Option<Server>,
    manager: ManuallyDrop<Manager>,
}

impl Serving {
    /// Closes the server as [`Server::close`] does.
    async fn close(&mut self) {
        let server = self.server.take().expect("invariant: one close");
        server.close(&self.manager).await;
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        if thread::panicking() {
            return;
        }
        if let Some(server) = self.server.take() {
            server.close_now(&self.manager);
        }
        // SAFETY: the server is deleted, and nothing drops the manager again.
        unsafe { ManuallyDrop::drop(&mut self.manager) };
    }
}

/// The limits of the channels of a [`Server`], each 0 for the default of open62541:
/// 65536 bytes, 512 MiB, and 16384 chunks.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Limits {
    /// The send and receive buffer size.
    pub(crate) buffer: u32,
    /// The `MaxMessageSize`.
    pub(crate) message: u32,
    /// The `MaxChunkCount`.
    pub(crate) chunks: u32,
}

/// The count of the Browse, `CreateMonitoredItems`, and Publish requests of a
/// [`Server`], and the most continuation points that one of its sessions held, from
/// [`Server::count`].
#[cfg(test)]
pub(crate) struct Counts<'s> {
    /// Lives until the delete of the server.
    counts: NonNull<ffi::test::Counts>,
    server: std::marker::PhantomData<&'s Server>,
}

#[cfg(test)]
impl Counts<'_> {
    /// Gives the count of the Browse requests.
    pub(crate) fn browses(&self) -> usize {
        // SAFETY: the server lives, and runs on this thread.
        unsafe { self.counts.as_ref().browses }
    }

    /// Gives the most continuation points that one session held when the server
    /// processed a request, from the count.
    pub(crate) fn points(&self) -> usize {
        // SAFETY: as `browses`.
        unsafe { self.counts.as_ref().points }
    }

    /// Gives the count of the `CreateMonitoredItems` requests.
    pub(crate) fn monitors(&self) -> usize {
        // SAFETY: as `browses`.
        unsafe { self.counts.as_ref().monitors }
    }

    /// Gives the count of the Publish requests.
    pub(crate) fn publishes(&self) -> usize {
        // SAFETY: as `browses`.
        unsafe { self.counts.as_ref().publishes }
    }

    /// Gives the count of the Publish requests answered with
    /// `BadTooManyPublishRequests`.
    pub(crate) fn refused(&self) -> usize {
        // SAFETY: as `browses`.
        unsafe { self.counts.as_ref().refused }
    }
}

/// A variable of [`Server::add_variable`].
pub(crate) struct Variable<'a> {
    /// Its node, whose key is not the number 0.
    pub(crate) node: Node,
    /// The node that it is under, by `reference`.
    pub(crate) parent: Node,
    /// Its reference type from `parent`, by its number in namespace 0.
    pub(crate) reference: u32,
    /// Its browse and display name.
    pub(crate) name: &'a CStr,
    /// Its `DataType`.
    pub(crate) data_type: Node,
    /// Its `ValueRank`. The rank 1 also gives it the `ArrayDimensions` 0.
    pub(crate) rank: i32,
    /// Its `AccessLevel` and `UserAccessLevel`.
    pub(crate) access: u8,
    /// The OPC UA binary encoding of its value, a `Variant`, or `None` for no value.
    pub(crate) value: Option<Vec<u8>>,
}

/// The `AccessLevel` bit `CurrentRead`.
pub(crate) const READ: u8 = 1;

/// The scalar variable `node` under the Objects folder by `Organizes`, of the browse
/// name `name`, the `DataType` of the number `data_type` in namespace 0, the access
/// [`READ`], and the encoded `Variant` `value`.
fn objects(
    node: &Node,
    name: &'static CStr,
    data_type: u32,
    value: Vec<u8>,
) -> Variable<'static> {
    Variable {
        node: node.clone(),
        parent: Node::ns0(ns0::OBJECTS),
        reference: ns0::ORGANIZES,
        name,
        data_type: Node::ns0(data_type),
        rank: -1,
        access: READ,
        value: Some(value),
    }
}

/// The OPC UA binary encoding of a scalar `Variant` of the built-in type of the number
/// `id` in namespace 0, whose value has the encoding `encoded`.
///
/// # Panics
///
/// If `id` is past 63, which the mask of a `Variant` cannot hold.
pub(crate) fn scalar(id: u32, encoded: &[u8]) -> Vec<u8> {
    let mask = u8::try_from(id)
        .ok()
        .filter(|mask| *mask < 64)
        .expect("a built-in type of at most 63");
    [&[mask][..], encoded].concat()
}

/// The OPC UA binary encoding of a `Variant` of the Doubles 1.5 and 2.5.
pub(crate) fn doubles() -> Vec<u8> {
    let mask = u8::try_from(ns0::DOUBLE).expect("invariant: Double is 11") | 0x80;
    [
        &[mask][..],
        &2_i32.to_le_bytes(),
        &1.5_f64.to_le_bytes(),
        &2.5_f64.to_le_bytes(),
    ]
    .concat()
}

/// The number in namespace 0 of the built-in type of `value`, and the OPC UA binary
/// encoding of a scalar `Variant` that holds `value`.
pub(crate) fn variant(value: Value) -> (u32, Vec<u8>) {
    let (id, encoded) = match value {
        Value::Bool(boolean) => (1, vec![u8::from(boolean)]),
        Value::I8(sbyte) => (2, sbyte.to_le_bytes().to_vec()),
        Value::U8(byte) => (3, vec![byte]),
        Value::I16(int16) => (4, int16.to_le_bytes().to_vec()),
        Value::U16(uint16) => (5, uint16.to_le_bytes().to_vec()),
        Value::I32(int32) => (6, int32.to_le_bytes().to_vec()),
        Value::U32(uint32) => (7, uint32.to_le_bytes().to_vec()),
        Value::I64(int64) => (8, int64.to_le_bytes().to_vec()),
        Value::U64(uint64) => (9, uint64.to_le_bytes().to_vec()),
        Value::F32(float) => (10, float.to_le_bytes().to_vec()),
        Value::F64(double) => (11, double.to_le_bytes().to_vec()),
    };
    (id, scalar(id, &encoded))
}

/// `node` for an add.
///
/// # Panics
///
/// If its key is the number 0, since open62541 then adds the node at a free number.
fn added(node: &Node) -> ffi::Node<'_> {
    assert!(
        node.key != Key::Number(0),
        "{node}: open62541 adds a node of the number 0 at a free number"
    );
    node.into()
}

#[cfg(test)]
mod tests;
