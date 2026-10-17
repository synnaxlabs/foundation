/* The symbols that the copy leaves undefined when it is built with no architecture, the
   event loop that `src/event.rs` gives the copy, and the connection manager that
   `src/connection.rs` gives it. */

/* The headers of the copy have unused parameters. Any other warning in them fails the
   build. */
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wunused-parameter"
#include <open62541/client_config_default.h>
#include <open62541/client_highlevel_async.h>
#include <open62541/plugin/certificategroup_default.h>
#include <open62541/plugin/eventloop.h>
#include <open62541/plugin/securitypolicy_default.h>
#include <open62541/types.h>
#include <open62541/util.h>
#include "mp_printf.h"
#include "timer.h"
#pragma GCC diagnostic pop

#include "shim.h"

#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#ifdef _WIN32
#include <windows.h>
#endif

/* The global clocks give a fixed time, so no OS clock enters through the C code.
 * `cargo xtask open62541` lists each call site. */
UA_DateTime UA_DateTime_now(void) { return 0; }
UA_DateTime UA_DateTime_nowMonotonic(void) { return 0; }
UA_Int64 UA_DateTime_localTimeUtcOffset(void) { return 0; }

/* The bytes of a line to stderr, with its newline. */
#define LINE_BYTES 512

/* Writes `connector-opcua: open62541 `, then `level` and `: ` when `level` is not NULL,
 * then `format` with `args`, to stderr as one line of at most `LINE_BYTES` bytes, in
 * one call, with the same bytes on each OS. So a line takes no `stdio` lock and does
 * not mix with a line of another thread. */
static void put_line(const char *level, const char *format, va_list args) {
    char line[LINE_BYTES];
    /* `mp_vsnprintf`, not `vsnprintf`: open62541 formats `%S` and `%N`. */
    int length = mp_snprintf(line, sizeof(line), "connector-opcua: open62541 %s%s",
                             level ? level : "", level ? ": " : "");
    length += mp_vsnprintf(line + length, sizeof(line) - (size_t)length, format, args);
    if(length > LINE_BYTES - 1)
        length = LINE_BYTES - 1;
    line[length] = '\n';
    /* A failed write of a line to stderr has nowhere to go. */
#ifdef _WIN32
    /* `write` puts a CR before each LF of fd 2, which is in text mode. */
    DWORD written;
    HANDLE err = GetStdHandle(STD_ERROR_HANDLE);
    WriteFile(err, line, (DWORD)length + 1, &written, NULL);
#else
    ssize_t written = write(STDERR_FILENO, line, (size_t)length + 1);
#endif
    (void)written;
}

/* Writes `format` with its arguments to stderr with `put_line`, and aborts. */
static void die(const char *format, ...) {
    va_list args;
    va_start(args, format);
    put_line(NULL, format, args);
    va_end(args);
    abort();
}

/* The library calls the constructors of the POSIX loop only when a config has no event
 * loop, and ours always has one. `cargo xtask open62541` omits the file stores and the
 * deprecated and ECC policies, and the production code adds none. So a call is a
 * defect. */
static void refuse(const char *name) { die("called %s, which is not built", name); }

UA_EventLoop *UA_EventLoop_new_POSIX(const UA_Logger *logger) {
    (void)logger;
    refuse("UA_EventLoop_new_POSIX");
    return NULL;
}

UA_ConnectionManager *UA_ConnectionManager_new_POSIX_TCP(const UA_String name) {
    (void)name;
    refuse("UA_ConnectionManager_new_POSIX_TCP");
    return NULL;
}

UA_ConnectionManager *UA_ConnectionManager_new_POSIX_UDP(const UA_String name) {
    (void)name;
    refuse("UA_ConnectionManager_new_POSIX_UDP");
    return NULL;
}

UA_ConnectionManager *UA_ConnectionManager_new_POSIX_Ethernet(const UA_String name) {
    (void)name;
    refuse("UA_ConnectionManager_new_POSIX_Ethernet");
    return NULL;
}

UA_InterruptManager *UA_InterruptManager_new_POSIX(const UA_String name) {
    (void)name;
    refuse("UA_InterruptManager_new_POSIX");
    return NULL;
}

#define REFUSE_RSA(name)                                                              \
    UA_StatusCode name(UA_SecurityPolicy *policy, const UA_ByteString certificate,     \
                       const UA_ByteString key, const UA_Logger *logger) {             \
        (void)policy, (void)certificate, (void)key, (void)logger;                      \
        refuse(#name);                                                                 \
        return UA_STATUSCODE_BADINTERNALERROR;                                         \
    }
#define REFUSE_ECC(name)                                                              \
    UA_StatusCode name(UA_SecurityPolicy *policy, const UA_ApplicationType type,       \
                       const UA_ByteString certificate, const UA_ByteString key,       \
                       const UA_Logger *logger) {                                      \
        (void)policy, (void)type, (void)certificate, (void)key, (void)logger;          \
        refuse(#name);                                                                 \
        return UA_STATUSCODE_BADINTERNALERROR;                                         \
    }
REFUSE_RSA(UA_SecurityPolicy_Basic128Rsa15)
REFUSE_RSA(UA_SecurityPolicy_Basic256)
REFUSE_ECC(UA_SecurityPolicy_EccBrainpoolP256r1)
REFUSE_ECC(UA_SecurityPolicy_EccBrainpoolP384r1)
REFUSE_ECC(UA_SecurityPolicy_EccCurve25519)
REFUSE_ECC(UA_SecurityPolicy_EccCurve448)
REFUSE_ECC(UA_SecurityPolicy_EccNistP256)
REFUSE_ECC(UA_SecurityPolicy_EccNistP256AesGcm)
REFUSE_ECC(UA_SecurityPolicy_EccNistP256ChaChaPoly)
REFUSE_ECC(UA_SecurityPolicy_EccNistP384)

#if defined(__linux__) || defined(UA_ARCHITECTURE_WIN32)
UA_StatusCode UA_CertificateGroup_Filestore(UA_CertificateGroup *certGroup,
                                            UA_NodeId *certificateGroupId,
                                            const UA_String storePath,
                                            const UA_Logger *logger,
                                            const UA_KeyValueMap *params) {
    (void)certGroup, (void)certificateGroupId, (void)storePath, (void)logger,
        (void)params;
    refuse("UA_CertificateGroup_Filestore");
    return UA_STATUSCODE_BADINTERNALERROR;
}

UA_StatusCode UA_SecurityPolicy_Filestore(UA_SecurityPolicy *policy,
                                          UA_SecurityPolicy *innerPolicy,
                                          const UA_String storePath) {
    (void)policy, (void)innerPolicy, (void)storePath;
    refuse("UA_SecurityPolicy_Filestore");
    return UA_STATUSCODE_BADINTERNALERROR;
}
#endif

/* `src/ffi.rs` mirrors these and asserts the same sizes. */
_Static_assert(sizeof(UA_NodeId) == 24, "UA_NodeId changed");
_Static_assert(sizeof(UA_DataType) == 96, "UA_DataType changed");
_Static_assert(sizeof(UA_DecodeBinaryOptions) == 40, "UA_DecodeBinaryOptions changed");
_Static_assert(offsetof(UA_DecodeBinaryOptions, decodedLength) == 32,
               "decodedLength moved");
_Static_assert(sizeof(shim_node) == 24, "shim_node changed");
_Static_assert(offsetof(shim_node, name) == 8, "shim_node.name moved");
_Static_assert(sizeof(shim_scalar) == 8, "shim_scalar changed");
_Static_assert(UA_TYPES_COUNT == 388, "UA_TYPES changed");
_Static_assert(UA_TYPES_BOOLEAN == 0 && UA_TYPES_SBYTE == 1 &&
                   UA_TYPES_BYTE == 2 && UA_TYPES_INT16 == 3 &&
                   UA_TYPES_UINT16 == 4 && UA_TYPES_INT32 == 5 &&
                   UA_TYPES_UINT32 == 6 && UA_TYPES_INT64 == 7 &&
                   UA_TYPES_UINT64 == 8 && UA_TYPES_FLOAT == 9 &&
                   UA_TYPES_DOUBLE == 10,
               "the scalar types of UA_TYPES moved");
_Static_assert(UA_NS0ID_BOOLEAN == 1 && UA_NS0ID_SBYTE == 2 &&
                   UA_NS0ID_BYTE == 3 && UA_NS0ID_INT16 == 4 &&
                   UA_NS0ID_UINT16 == 5 && UA_NS0ID_INT32 == 6 &&
                   UA_NS0ID_UINT32 == 7 && UA_NS0ID_INT64 == 8 &&
                   UA_NS0ID_UINT64 == 9 && UA_NS0ID_FLOAT == 10 &&
                   UA_NS0ID_DOUBLE == 11,
               "the id of a scalar type is not its index in UA_TYPES plus 1");
_Static_assert(UA_TYPES_STRING == UA_NS0ID_STRING - 1 &&
                   UA_TYPES_DATETIME == UA_NS0ID_DATETIME - 1 &&
                   UA_TYPES_GUID == UA_NS0ID_GUID - 1 &&
                   UA_TYPES_BYTESTRING == UA_NS0ID_BYTESTRING - 1 &&
                   UA_TYPES_XMLELEMENT == UA_NS0ID_XMLELEMENT - 1 &&
                   UA_TYPES_NODEID == UA_NS0ID_NODEID - 1 &&
                   UA_TYPES_EXPANDEDNODEID == UA_NS0ID_EXPANDEDNODEID - 1 &&
                   UA_TYPES_STATUSCODE == UA_NS0ID_STATUSCODE - 1 &&
                   UA_TYPES_QUALIFIEDNAME == UA_NS0ID_QUALIFIEDNAME - 1 &&
                   UA_TYPES_LOCALIZEDTEXT == UA_NS0ID_LOCALIZEDTEXT - 1 &&
                   UA_TYPES_DATAVALUE == UA_NS0ID_DATAVALUE - 1 &&
                   UA_TYPES_DIAGNOSTICINFO == UA_NS0ID_DIAGNOSTICINFO - 1,
               "the id of a built-in type is not its index in UA_TYPES plus 1");
_Static_assert(SHIM_ARRAY == 0x8000, "SHIM_ARRAY is `ffi::ARRAY`");
_Static_assert(UA_DATATYPEKIND_BOOLEAN == 0 && UA_DATATYPEKIND_SBYTE == 1 &&
                   UA_DATATYPEKIND_BYTE == 2 && UA_DATATYPEKIND_INT16 == 3 &&
                   UA_DATATYPEKIND_UINT16 == 4 && UA_DATATYPEKIND_INT32 == 5 &&
                   UA_DATATYPEKIND_UINT32 == 6 && UA_DATATYPEKIND_INT64 == 7 &&
                   UA_DATATYPEKIND_UINT64 == 8 && UA_DATATYPEKIND_FLOAT == 9 &&
                   UA_DATATYPEKIND_DOUBLE == 10,
               "the kinds of the scalar types moved");
_Static_assert(UA_TYPES_BYTESTRING == 14, "UA_TYPES_BYTESTRING moved");
_Static_assert(UA_TYPES_VARIANT == 23, "UA_TYPES_VARIANT moved");
_Static_assert(UA_NS0ID_SERVER_SERVERSTATUS_CURRENTTIME == 2258,
               "UA_NS0ID_SERVER_SERVERSTATUS_CURRENTTIME moved");
_Static_assert(
    UA_NS0ID_SERVER_SERVERCAPABILITIES_OPERATIONLIMITS_MAXNODESPERREAD == 11705,
    "UA_NS0ID_SERVER_SERVERCAPABILITIES_OPERATIONLIMITS_MAXNODESPERREAD moved");
_Static_assert(UA_NS0ID_SERVER_SERVERSTATUS_STATE == 2259,
               "UA_NS0ID_SERVER_SERVERSTATUS_STATE moved");
_Static_assert(UA_SECURECHANNELSTATE_CLOSED == 0 && UA_SECURECHANNELSTATE_OPEN == 11,
               "UA_SecureChannelState moved");
_Static_assert(sizeof(UA_SecureChannelState) == sizeof(int),
               "UA_SecureChannelState changed");
_Static_assert(UA_TIMERPOLICY_ONCE == 0 && UA_TIMERPOLICY_CURRENTTIME == 1 &&
                   UA_TIMERPOLICY_BASETIME == 2,
               "UA_TimerPolicy moved");
_Static_assert(sizeof(UA_TimerPolicy) == sizeof(int), "UA_TimerPolicy changed");
_Static_assert(UA_EVENTLOOPSTATE_FRESH == 0 && UA_EVENTLOOPSTATE_STARTED == 2,
               "UA_EventLoopState moved");
_Static_assert(sizeof(UA_EventLoopState) == sizeof(int), "UA_EventLoopState changed");
_Static_assert(UA_CONNECTIONSTATE_OPENING == 1 && UA_CONNECTIONSTATE_ESTABLISHED == 2 &&
                   UA_CONNECTIONSTATE_CLOSING == 3,
               "UA_ConnectionState moved");
_Static_assert(sizeof(UA_ConnectionState) == sizeof(int), "UA_ConnectionState changed");
_Static_assert(UA_LIFECYCLESTATE_STOPPED == 0 && UA_LIFECYCLESTATE_STOPPING == 2,
               "UA_LifecycleState moved");
_Static_assert(sizeof(UA_LifecycleState) == sizeof(int), "UA_LifecycleState changed");
_Static_assert(UA_NODEIDTYPE_NUMERIC == 0, "UA_NODEIDTYPE_NUMERIC moved");
_Static_assert(sizeof(enum UA_NodeIdType) == sizeof(int), "UA_NodeIdType changed");
_Static_assert(sizeof(UA_SessionState) == sizeof(int), "UA_SessionState changed");
_Static_assert(UA_SESSIONSTATE_CREATE_REQUESTED == 1,
               "UA_SESSIONSTATE_CREATE_REQUESTED moved");
_Static_assert(UA_SESSIONSTATE_ACTIVATED == 4, "UA_SESSIONSTATE_ACTIVATED moved");
_Static_assert(sizeof(UA_EventSourceType) == sizeof(int), "UA_EventSourceType changed");
_Static_assert(sizeof(UA_EventSourceState) == sizeof(int),
               "UA_EventSourceState changed");

/* `src/ffi.rs` mirrors the struct, in words of the size of a pointer, and asserts the
 * same offsets. */
_Static_assert(sizeof(UA_EventLoop) == 23 * sizeof(void *), "UA_EventLoop changed");
#define AT(member, word)                                                               \
    _Static_assert(offsetof(UA_EventLoop, member) == (word) * sizeof(void *),         \
                   "UA_EventLoop." #member " moved")
AT(logger, 0);
AT(params, 1);
AT(state, 3);
AT(start, 4);
AT(stop, 5);
AT(free, 6);
AT(run, 7);
AT(cancel, 8);
AT(dateTime_now, 9);
AT(dateTime_nowMonotonic, 10);
AT(dateTime_localTimeUtcOffset, 11);
AT(nextTimer, 12);
AT(addTimer, 13);
AT(modifyTimer, 14);
AT(removeTimer, 15);
AT(addDelayedCallback, 16);
AT(removeDelayedCallback, 17);
AT(eventSources, 18);
AT(registerEventSource, 19);
AT(deregisterEventSource, 20);
AT(lock, 21);
AT(unlock, 22);
#undef AT

/* The last tick of a clock of `u64` ns. */
#define LAST_TICK ((UA_DateTime)(UINT64_MAX / 100))

/* Gives a time of `time` in ticks of 100 ns: the monotonic time, at most `LAST_TICK`,
 * or the wall time from the Unix epoch. */
typedef UA_DateTime (*shim_now)(void *time);

/* An event loop with no I/O. It runs on one thread and never blocks. */
struct shim_loop {
    /* First, so a pointer to it is a pointer to the loop. */
    UA_EventLoop el;
    UA_Timer timer;
    UA_Logger logger;
    shim_now now;
    shim_now wall;
    void *time;
    /* The delayed callbacks for the next run, in order, and `tail` at the `next` of
     * the last. */
    UA_DelayedCallback *queued;
    UA_DelayedCallback **tail;
    /* The delayed callbacks of the run in progress that have not run yet. */
    UA_DelayedCallback *running;
    UA_Boolean executing;
};

static struct shim_loop *loop_of(UA_EventLoop *el) { return (struct shim_loop *)el; }

static UA_DateTime now_of(UA_EventLoop *el) {
    struct shim_loop *loop = loop_of(el);
    return loop->now(loop->time);
}

static void set_state(UA_EventLoop *el, UA_EventLoopState state) {
    *(UA_EventLoopState *)(uintptr_t)&el->state = state;
}

/* A client or server with an external loop never calls these. */
static void el_stop(UA_EventLoop *el) {
    (void)el;
    refuse("UA_EventLoop.stop");
}

static UA_StatusCode el_free(UA_EventLoop *el) {
    (void)el;
    refuse("UA_EventLoop.free");
    return UA_STATUSCODE_BADINTERNALERROR;
}

static UA_StatusCode el_register(UA_EventLoop *el, UA_EventSource *es) {
    (void)el;
    (void)es;
    refuse("UA_EventLoop.registerEventSource");
    return UA_STATUSCODE_BADINTERNALERROR;
}

static UA_StatusCode el_deregister(UA_EventLoop *el, UA_EventSource *es) {
    (void)el;
    (void)es;
    refuse("UA_EventLoop.deregisterEventSource");
    return UA_STATUSCODE_BADINTERNALERROR;
}

static UA_StatusCode el_start(UA_EventLoop *el) {
    if(el->state != UA_EVENTLOOPSTATE_FRESH)
        return UA_STATUSCODE_BADINTERNALERROR;
    set_state(el, UA_EVENTLOOPSTATE_STARTED);
    return UA_STATUSCODE_GOOD;
}

/* Runs the queued delayed callbacks. Those that they queue wait for the next pass. */
static void run_delayed(struct shim_loop *loop) {
    loop->running = loop->queued;
    loop->queued = NULL;
    loop->tail = &loop->queued;
    while(loop->running) {
        UA_DelayedCallback *dc = loop->running;
        loop->running = dc->next;
        dc->next = NULL;
        /* It may free `dc`. */
        dc->callback(dc->application, dc->context);
    }
}

/* Runs the due timers, then the delayed callbacks queued before the call or by those
 * timers. */
static UA_StatusCode el_run(UA_EventLoop *el, UA_UInt32 timeout) {
    (void)timeout;
    struct shim_loop *loop = loop_of(el);
    if(el->state != UA_EVENTLOOPSTATE_STARTED || loop->executing)
        return UA_STATUSCODE_BADINTERNALERROR;
    loop->executing = true;
    UA_Timer_process(&loop->timer, now_of(el));
    run_delayed(loop);
    loop->executing = false;
    return UA_STATUSCODE_GOOD;
}

/* Wakes a loop that waits, and this one never does. */
static void el_cancel(UA_EventLoop *el) { (void)el; }

static UA_DateTime el_now(UA_EventLoop *el) {
    struct shim_loop *loop = loop_of(el);
    return loop->wall(loop->time) + UA_DATETIME_UNIX_EPOCH;
}

static UA_Int64 el_utc_offset(UA_EventLoop *el) {
    (void)el;
    return 0;
}

static UA_DateTime el_next_timer(UA_EventLoop *el) {
    struct shim_loop *loop = loop_of(el);
    if(loop->queued)
        return now_of(el);
    return UA_Timer_next(&loop->timer);
}

/* Whether the ticks of `interval_ms`, their due time from `now` give or take the 1 s
   that the copy may move it by to batch timers, `base`, and the distance from `base`
   to `now` fit in `UA_DateTime`. False for NaN. For a repeated timer, the due time is
   from `LAST_TICK`: a run, at most at it, adds the interval. The copy casts, adds, and
   subtracts them unchecked, and a once timer is due at its base. `now` is never
   negative, so ticks above `-room` give a due time above it too. */
static UA_Boolean in_range(UA_DateTime now, UA_Double interval_ms,
                           const UA_DateTime *base, UA_TimerPolicy policy) {
    const UA_DateTime room = UA_INT64_MAX - UA_DATETIME_SEC;
    UA_DateTime from = policy == UA_TIMERPOLICY_ONCE ? now : LAST_TICK;
    UA_Double ticks = interval_ms * UA_DATETIME_MSEC;
    return ticks < (UA_Double)room - (UA_Double)from && ticks > -(UA_Double)room &&
           (!base || (*base >= now - room && *base < room));
}

static UA_StatusCode el_add_timer(UA_EventLoop *el, UA_Callback cb, void *application,
                                  void *data, UA_Double interval_ms,
                                  UA_DateTime *base, UA_TimerPolicy policy,
                                  UA_UInt64 *key) {
    UA_DateTime now = now_of(el);
    if(!in_range(now, interval_ms, base, policy))
        return UA_STATUSCODE_BADOUTOFRANGE;
    return UA_Timer_add(&loop_of(el)->timer, cb, application, data, interval_ms, now,
                        base, policy, key);
}

static UA_StatusCode el_modify_timer(UA_EventLoop *el, UA_UInt64 key,
                                     UA_Double interval_ms, UA_DateTime *base,
                                     UA_TimerPolicy policy) {
    UA_DateTime now = now_of(el);
    if(!in_range(now, interval_ms, base, policy))
        return UA_STATUSCODE_BADOUTOFRANGE;
    return UA_Timer_modify(&loop_of(el)->timer, key, interval_ms, now, base, policy);
}

static void el_remove_timer(UA_EventLoop *el, UA_UInt64 key) {
    UA_Timer_remove(&loop_of(el)->timer, key);
}

static void el_add_delayed(UA_EventLoop *el, UA_DelayedCallback *dc) {
    struct shim_loop *loop = loop_of(el);
    dc->next = NULL;
    *loop->tail = dc;
    loop->tail = &dc->next;
}

/* Unlinks `dc` from the list at `head`, and gives whether it was there. */
static UA_Boolean unlink_delayed(UA_DelayedCallback **head, UA_DelayedCallback *dc) {
    for(; *head; head = &(*head)->next) {
        if(*head == dc) {
            *head = dc->next;
            dc->next = NULL;
            return true;
        }
    }
    return false;
}

static void el_remove_delayed(UA_EventLoop *el, UA_DelayedCallback *dc) {
    struct shim_loop *loop = loop_of(el);
    if(unlink_delayed(&loop->running, dc))
        return;
    UA_DelayedCallback *next = dc->next;
    if(unlink_delayed(&loop->queued, dc) && !next) {
        loop->tail = &loop->queued;
        while(*loop->tail)
            loop->tail = &(*loop->tail)->next;
    }
}

static const char *const LEVELS[] = {"trace", "debug", "info", "warning", "error",
                                     "fatal"};

/* Writes each message of level warning and up to stderr. It allocates nothing. */
static void log_message(void *context, UA_LogLevel level, UA_LogCategory category,
                        const char *msg, va_list args) {
    (void)context;
    (void)category;
    if(level < UA_LOGLEVEL_WARNING)
        return;
    put_line(LEVELS[level / 100 - 1], msg, args);
}

/* Logs the `length` bytes at `message` as a warning through the logger of `el`.
 * `length` is above 0: mp_printf reads a precision of 0 as none, and reads to a NUL. */
void shim_log_warning(UA_EventLoop *el, const char *message, size_t length) {
    int bytes = length < LINE_BYTES ? (int)length : LINE_BYTES;
    UA_LOG_WARNING(el->logger, UA_LOGCATEGORY_NETWORK, "%.*s", bytes, message);
}

/* Gives a fresh loop whose monotonic time is `now(time)` and whose wall time is
 * `wall(time)`, or NULL when out of memory. */
UA_EventLoop *shim_loop_new(shim_now now, shim_now wall, void *time) {
    struct shim_loop *loop = (struct shim_loop *)UA_calloc(1, sizeof(*loop));
    if(!loop)
        return NULL;
    UA_Timer_init(&loop->timer);
    loop->now = now;
    loop->wall = wall;
    loop->time = time;
    loop->tail = &loop->queued;
    loop->logger.log = log_message;
    UA_EventLoop *el = &loop->el;
    el->logger = &loop->logger;
    el->start = el_start;
    el->stop = el_stop;
    el->free = el_free;
    el->run = el_run;
    el->cancel = el_cancel;
    el->dateTime_now = el_now;
    el->dateTime_nowMonotonic = now_of;
    el->dateTime_localTimeUtcOffset = el_utc_offset;
    el->nextTimer = el_next_timer;
    el->addTimer = el_add_timer;
    el->modifyTimer = el_modify_timer;
    el->removeTimer = el_remove_timer;
    el->addDelayedCallback = el_add_delayed;
    el->removeDelayedCallback = el_remove_delayed;
    el->registerEventSource = el_register;
    el->deregisterEventSource = el_deregister;
    return el;
}

/* Whether a delayed callback of `el`, a loop of `shim_loop_new`, waits for the next
 * run. */
UA_Boolean shim_loop_delayed(UA_EventLoop *el) { return loop_of(el)->queued != NULL; }

/* The most passes of delayed callbacks that a free runs. A callback that queues
 * itself at each pass is a defect. */
#define FREE_PASSES 64

/* `FREE_PASSES`, for the close of a server in `src/server.rs`. */
const int shim_free_passes = FREE_PASSES;

/* Runs the queued delayed callbacks, which free what they hold, and those that they
 * queue, removes each timer, and frees `el`, a loop of `shim_loop_new`. Aborts when
 * callbacks are still queued after `FREE_PASSES` passes. */
void shim_loop_free(UA_EventLoop *el) {
    struct shim_loop *loop = loop_of(el);
    for(int pass = 0; loop->queued; pass++) {
        if(pass == FREE_PASSES)
            die("queued delayed callbacks for %d passes of a loop free", FREE_PASSES);
        run_delayed(loop);
    }
    UA_Timer_clear(&loop->timer);
    UA_free(loop);
}

/* The hooks of `src/connection.rs`. Each takes the `state` of `shim_cm_new`. */
typedef struct {
    UA_StatusCode (*open)(void *state, UA_String host, UA_UInt16 port, void *application,
                          void *context, UA_ConnectionManager_connectionCallback callback);
    UA_StatusCode (*listen)(void *state, UA_String host, UA_UInt16 port,
                            void *application, void *context,
                            UA_ConnectionManager_connectionCallback callback);
    UA_StatusCode (*send)(void *state, uintptr_t id, UA_ByteString *buffer);
    UA_StatusCode (*close)(void *state, uintptr_t id);
} shim_hooks;

/* A TCP connection manager whose connections live in Rust. */
struct shim_cm {
    /* First, so a pointer to it is a pointer to the manager. */
    UA_ConnectionManager cm;
    const shim_hooks *hooks;
    void *state;
};

/* `src/ffi/test.rs` mirrors the struct, and asserts the same offsets. */
_Static_assert(sizeof(UA_ConnectionManager) == 18 * sizeof(void *),
               "UA_ConnectionManager changed");
#define AT(member, word)                                                               \
    _Static_assert(offsetof(UA_ConnectionManager, member) == (word) * sizeof(void *), \
                   "UA_ConnectionManager." #member " moved")
AT(eventSource.eventSourceType, 1);
AT(eventSource.eventLoop, 4);
AT(eventSource.state, 7);
AT(protocol, 11);
AT(openConnection, 13);
AT(sendWithConnection, 14);
AT(closeConnection, 15);
AT(allocNetworkBuffer, 16);
AT(freeNetworkBuffer, 17);
#undef AT

static struct shim_cm *cm_of(UA_ConnectionManager *cm) { return (struct shim_cm *)cm; }

/* A client or server with an external loop never calls these. */
static UA_StatusCode cm_start(UA_EventSource *es) {
    (void)es;
    refuse("UA_ConnectionManager.start");
    return UA_STATUSCODE_BADINTERNALERROR;
}

static void cm_stop(UA_EventSource *es) {
    (void)es;
    refuse("UA_ConnectionManager.stop");
}

static UA_StatusCode cm_free(UA_EventSource *es) {
    (void)es;
    refuse("UA_ConnectionManager.free");
    return UA_STATUSCODE_BADINTERNALERROR;
}

/* Listens on the `port` of `params`, or opens a client connection to its `address`
 * and `port`. The listener of the manager sets the address of a listen, which names
 * its host in `address` as a string or an array of at most one, since the manager
 * has one listener. */
static UA_StatusCode cm_open(UA_ConnectionManager *cm, const UA_KeyValueMap *params,
                             void *application, void *context,
                             UA_ConnectionManager_connectionCallback callback) {
    struct shim_cm *s = cm_of(cm);
    const UA_Boolean *listen = (const UA_Boolean *)UA_KeyValueMap_getScalar(
        params, UA_QUALIFIEDNAME(0, "listen"), &UA_TYPES[UA_TYPES_BOOLEAN]);
    const UA_UInt16 *port = (const UA_UInt16 *)UA_KeyValueMap_getScalar(
        params, UA_QUALIFIEDNAME(0, "port"), &UA_TYPES[UA_TYPES_UINT16]);
    if(!port)
        return UA_STATUSCODE_BADINVALIDARGUMENT;
    if(listen && *listen) {
        UA_String host = UA_STRING_NULL;
        const UA_Variant *hosts =
            UA_KeyValueMap_get(params, UA_QUALIFIEDNAME(0, "address"));
        if(hosts) {
            size_t size = UA_Variant_isScalar(hosts) ? 1 : hosts->arrayLength;
            if(hosts->type != &UA_TYPES[UA_TYPES_STRING] || size > 1)
                return UA_STATUSCODE_BADINVALIDARGUMENT;
            if(size == 1)
                host = *(const UA_String *)hosts->data;
        }
        return s->hooks->listen(s->state, host, *port, application, context, callback);
    }
    const UA_String *address = (const UA_String *)UA_KeyValueMap_getScalar(
        params, UA_QUALIFIEDNAME(0, "address"), &UA_TYPES[UA_TYPES_STRING]);
    if(!address)
        return UA_STATUSCODE_BADINVALIDARGUMENT;
    return s->hooks->open(s->state, *address, *port, application, context, callback);
}

static UA_StatusCode cm_send(UA_ConnectionManager *cm, uintptr_t id,
                             const UA_KeyValueMap *params, UA_ByteString *buffer) {
    (void)params;
    struct shim_cm *s = cm_of(cm);
    return s->hooks->send(s->state, id, buffer);
}

static UA_StatusCode cm_close(UA_ConnectionManager *cm, uintptr_t id) {
    struct shim_cm *s = cm_of(cm);
    return s->hooks->close(s->state, id);
}

/* Unlike `UA_ByteString_allocBuffer`, it does not zero the bytes: open62541 sends
 * only the bytes it writes. */
static UA_StatusCode cm_alloc(UA_ConnectionManager *cm, uintptr_t id,
                              UA_ByteString *buffer, size_t size) {
    (void)cm;
    (void)id;
    UA_ByteString_init(buffer);
    if(size == 0)
        return UA_STATUSCODE_GOOD;
    buffer->data = (UA_Byte *)UA_malloc(size);
    if(!buffer->data)
        return UA_STATUSCODE_BADOUTOFMEMORY;
    buffer->length = size;
    return UA_STATUSCODE_GOOD;
}

/* Frees a buffer of `allocNetworkBuffer`. Rust frees each buffer that a send gives it
 * with this. */
void shim_buffer_free(UA_ByteString *buffer) { UA_ByteString_clear(buffer); }

static void cm_free_buffer(UA_ConnectionManager *cm, uintptr_t id,
                           UA_ByteString *buffer) {
    (void)cm;
    (void)id;
    shim_buffer_free(buffer);
}

/* Gives a started TCP connection manager on `el`, first in its event sources, whose
 * calls go to `hooks` with `state`, or NULL when out of memory. */
UA_ConnectionManager *shim_cm_new(UA_EventLoop *el, const shim_hooks *hooks,
                                  void *state) {
    struct shim_cm *s = (struct shim_cm *)UA_calloc(1, sizeof(*s));
    if(!s)
        return NULL;
    s->hooks = hooks;
    s->state = state;
    UA_ConnectionManager *cm = &s->cm;
    UA_EventSource *es = &cm->eventSource;
    es->eventSourceType = UA_EVENTSOURCETYPE_CONNECTIONMANAGER;
    es->name = (UA_String)UA_STRING_STATIC("tcp connection manager");
    es->eventLoop = el;
    es->state = UA_EVENTSOURCESTATE_STARTED;
    es->start = cm_start;
    es->stop = cm_stop;
    es->free = cm_free;
    cm->protocol = (UA_String)UA_STRING_STATIC("tcp");
    cm->openConnection = cm_open;
    cm->sendWithConnection = cm_send;
    cm->closeConnection = cm_close;
    cm->allocNetworkBuffer = cm_alloc;
    cm->freeNetworkBuffer = cm_free_buffer;
    es->next = el->eventSources;
    el->eventSources = es;
    return cm;
}

/* Unlinks `cm`, a manager of `shim_cm_new`, from its loop and frees it. */
void shim_cm_free(UA_ConnectionManager *cm) {
    UA_EventSource **at = &cm->eventSource.eventLoop->eventSources;
    while(*at != &cm->eventSource)
        at = &(*at)->next;
    *at = cm->eventSource.next;
    UA_free(cm);
}

/* Calls `callback` with `ESTABLISHED`, no message, and the string of the `length`
 * bytes at `address`: as `listen-address` with `port` as `listen-port` when `port`
 * is not NULL, else as `remote-address`. A `length` of 0 gives no params. The params
 * live on the stack. */
void shim_establish(UA_ConnectionManager *cm, uintptr_t id, void *application,
                    void **context, UA_ConnectionManager_connectionCallback callback,
                    const UA_Byte *address, size_t length, const UA_UInt16 *port) {
    UA_String text = {length, (UA_Byte *)(uintptr_t)address};
    UA_KeyValuePair pairs[2];
    UA_KeyValueMap params = {0, pairs};
    if(length > 0) {
        pairs[0].key = UA_QUALIFIEDNAME(0, port ? "listen-address" : "remote-address");
        UA_Variant_setScalar(&pairs[0].value, &text, &UA_TYPES[UA_TYPES_STRING]);
        params.mapSize = 1;
        if(port) {
            pairs[1].key = UA_QUALIFIEDNAME(0, "listen-port");
            UA_Variant_setScalar(&pairs[1].value, (void *)(uintptr_t)port,
                                 &UA_TYPES[UA_TYPES_UINT16]);
            params.mapSize = 2;
        }
    }
    UA_ByteString message = UA_BYTESTRING_NULL;
    callback(cm, id, application, context, UA_CONNECTIONSTATE_ESTABLISHED, &params,
             message);
}

/* Gives a client on `el` that sends and takes at most `chunks` chunks in a message,
 * or NULL when out of memory. A failed copy of the application URI or of the URI of
 * namespace 0 gives a client with that URI empty (#2566). */
UA_Client *shim_client_new(UA_EventLoop *el, UA_UInt32 chunks) {
    UA_ClientConfig config;
    memset(&config, 0, sizeof(config));
    config.logging = &loop_of(el)->logger;
    config.eventLoop = el;
    config.externalEventLoop = true;
    if(UA_ClientConfig_setDefault(&config) != UA_STATUSCODE_GOOD) {
        UA_ClientConfig_clear(&config);
        return NULL;
    }
    /* open62541 checks the chunks of a message that it sends and of one that it takes
     * against this count, not against the count of the peer. */
    config.localConnectionConfig.localMaxChunkCount = chunks;
    /* No custom types: `value::mistyped` finds the type of each decoded value in
     * `UA_TYPES`. */
    return UA_Client_newWithConfig(&config);
}

/* Gives the timeout of the requests of `client`, in ms. */
UA_UInt32 shim_client_timeout(UA_Client *client) {
    return UA_Client_getConfig(client)->timeout;
}

/* Writes to `limits` the send and receive buffer sizes of the channel of `client`, the
 * MaxMessageSize and the MaxChunkCount of its server (0 for none), and the encoded size
 * of the session token of `client`. Gives `BadInternalError` when the copy gives
 * another value than 5 `UInt32` values. */
UA_StatusCode shim_client_limits(UA_Client *client, size_t limits[5]) {
    UA_Variant value;
    UA_StatusCode status = UA_Client_getConnectionAttributeCopy(
        client, UA_QUALIFIEDNAME(0, "messageLimits"), &value);
    if(status != UA_STATUSCODE_GOOD)
        return status;
    if(value.type != &UA_TYPES[UA_TYPES_UINT32] || value.arrayLength != 5) {
        UA_Variant_clear(&value);
        return UA_STATUSCODE_BADINTERNALERROR;
    }
    for(size_t i = 0; i < 5; i++)
        limits[i] = ((const UA_UInt32 *)value.data)[i];
    UA_Variant_clear(&value);
    return UA_STATUSCODE_GOOD;
}

/* Stops the loop of `client`, so no call of the delete waits on the loop, then deletes
 * `client`. The loop must hold no other client or server. A later run of the loop
 * fails. The delete sends no CloseSession, so an active session stays on the server
 * until it times out. */
void shim_client_free(UA_Client *client) {
    set_state(UA_Client_getConfig(client)->eventLoop, UA_EVENTLOOPSTATE_STOPPED);
    UA_Client_delete(client);
}

/* Sets `key` of `map` to an array of the `size` strings at `strings`, at most 4. */
UA_StatusCode shim_map_set_strings(UA_KeyValueMap *map, const char *key,
                                   const char *const *strings, size_t size) {
    UA_String texts[4];
    if(size > 4)
        return UA_STATUSCODE_BADINVALIDARGUMENT;
    for(size_t i = 0; i < size; i++)
        texts[i] = UA_STRING((char *)(uintptr_t)strings[i]);
    UA_Variant value;
    /* An empty array points at the sentinel, else it reads as a scalar. */
    UA_Variant_setArray(&value, size > 0 ? texts : UA_EMPTY_ARRAY_SENTINEL, size,
                        &UA_TYPES[UA_TYPES_STRING]);
    return UA_KeyValueMap_set(map, UA_QUALIFIEDNAME(0, (char *)(uintptr_t)key), &value);
}

/* Gives the service result of `response`, a response of a service. */
UA_StatusCode shim_response_result(const void *response) {
    return ((const UA_ResponseHeader *)response)->serviceResult;
}

/* Gives a Read request of the attribute `attributes[i]`, or the Value when
 * `attributes` is NULL, of the node `nodes[i]` for each `i` below `count`, with no
 * timestamps, which copies their names, or NULL when an allocation fails. */
static UA_ReadRequest *read_new(const shim_node *nodes, const UA_UInt32 *attributes,
                                size_t count) {
    UA_ReadRequest *request = UA_ReadRequest_new();
    if(!request)
        return NULL;
    request->timestampsToReturn = UA_TIMESTAMPSTORETURN_NEITHER;
    request->nodesToRead =
        (UA_ReadValueId *)UA_Array_new(count, &UA_TYPES[UA_TYPES_READVALUEID]);
    if(!request->nodesToRead) {
        UA_ReadRequest_delete(request);
        return NULL;
    }
    request->nodesToReadSize = count;
    for(size_t i = 0; i < count; i++) {
        UA_ReadValueId *read = &request->nodesToRead[i];
        read->attributeId = attributes ? attributes[i] : UA_ATTRIBUTEID_VALUE;
        UA_NodeId id = shim_node_id(nodes[i]);
        if(UA_NodeId_copy(&id, &read->nodeId) != UA_STATUSCODE_GOOD) {
            UA_ReadRequest_delete(request);
            return NULL;
        }
    }
    return request;
}

/* Gives a Read request of the Value of each of the `count` nodes at `nodes`, as
 * `read_new` does. */
UA_ReadRequest *shim_read_new(const shim_node *nodes, size_t count) {
    return read_new(nodes, NULL, count);
}

void shim_read_free(UA_ReadRequest *request) { UA_ReadRequest_delete(request); }

/* Sends `request`, of the type `request_type`, on `client`, and has `callback` called
 * with `data` and the answer, of the type `answer_type`. Writes the id of the request to
 * `id`. The request times out only after 2^32 - 1 ms (49.7 days), so in practice it
 * ends at its answer or at the close of the channel. */
static UA_StatusCode send(UA_Client *client, void *request,
                          const UA_DataType *request_type,
                          UA_ClientAsyncServiceCallback callback,
                          const UA_DataType *answer_type, void *data, UA_UInt32 *id) {
    /* open62541 takes the timeout hint of a request from the config, and makes 0
     * 2^32 - 1 ms (`src/client/ua_client.c`, lines 460 and 915). The server gets the
     * hint 0. A send keeps the handle that it gives a request with handle 0. */
    UA_ClientConfig *config = UA_Client_getConfig(client);
    UA_UInt32 timeout = config->timeout;
    config->timeout = 0;
    ((UA_RequestHeader *)request)->requestHandle = 0;
    UA_StatusCode status = __UA_Client_AsyncService(client, request, request_type,
                                                    callback, answer_type, data, id);
    config->timeout = timeout;
    return status;
}

/* Sends `request` on `client` as `send` does, with `callback` for the answer. */
UA_StatusCode shim_client_read(UA_Client *client, UA_ReadRequest *request,
                               UA_ClientAsyncReadCallback callback, void *data,
                               UA_UInt32 *id) {
    return send(client, request, &UA_TYPES[UA_TYPES_READREQUEST],
                (UA_ClientAsyncServiceCallback)callback,
                &UA_TYPES[UA_TYPES_READRESPONSE], data, id);
}

/* Gives the count of results of `response`, a Read response. */
size_t shim_read_count(const UA_ReadResponse *response) {
    return response->resultsSize;
}

/* Gives result `index` of `response`, a Read response with more results than
 * `index`. */
const UA_DataValue *shim_read_result(const UA_ReadResponse *response, size_t index) {
    return &response->results[index];
}

/* Gives the status of `value`, an answer of a read: its status when it is not Good,
 * else `BadNoData` when it holds no value, else `Good`, also for a Good with a subcode.
 * A read callback gets `Good` whatever the answer holds. */
UA_StatusCode shim_value_status(const void *value) {
    const UA_DataValue *answer = (const UA_DataValue *)value;
    if(answer->hasStatus && !UA_StatusCode_isGood(answer->status))
        return answer->status;
    return answer->hasValue ? UA_STATUSCODE_GOOD : UA_STATUSCODE_BADNODATA;
}

/* Writes the kind of `value`, a Good answer of a read, to `kind`, plus `SHIM_ARRAY` for
 * an array, and to `out` its scalar when `shim_scalar` holds that kind, else its type.
 * Else gives the status of `shim_value_status`, or `BadNoData` for an empty Variant. */
UA_StatusCode shim_value_scalar(const void *value, shim_scalar *out, UA_UInt16 *kind) {
    UA_StatusCode status = shim_value_status(value);
    if(status != UA_STATUSCODE_GOOD)
        return status;
    const UA_Variant *variant = &((const UA_DataValue *)value)->value;
    /* Locals: with `-fno-strict-aliasing`, each store to `out` makes C load again. */
    const UA_DataType *type = variant->type;
    if(!UA_Variant_isScalar(variant)) {
        if(type == NULL)
            return UA_STATUSCODE_BADNODATA;
        *kind = (UA_UInt16)(type->typeKind | SHIM_ARRAY);
        out->type = type;
        return UA_STATUSCODE_GOOD;
    }
    UA_UInt32 typeKind = type->typeKind;
    *kind = (UA_UInt16)typeKind;
    /* A load of fixed size for each kind, since a `memcpy` of `memSize` calls glibc. */
    const void *data = variant->data;
    switch(typeKind) {
    case UA_DATATYPEKIND_BOOLEAN: out->boolean = *(const UA_Boolean *)data; break;
    case UA_DATATYPEKIND_SBYTE: out->sbyte = *(const UA_SByte *)data; break;
    case UA_DATATYPEKIND_BYTE: out->byte = *(const UA_Byte *)data; break;
    case UA_DATATYPEKIND_INT16: out->int16 = *(const UA_Int16 *)data; break;
    case UA_DATATYPEKIND_UINT16: out->uint16 = *(const UA_UInt16 *)data; break;
    case UA_DATATYPEKIND_INT32: out->int32 = *(const UA_Int32 *)data; break;
    case UA_DATATYPEKIND_UINT32: out->uint32 = *(const UA_UInt32 *)data; break;
    case UA_DATATYPEKIND_INT64: out->int64 = *(const UA_Int64 *)data; break;
    case UA_DATATYPEKIND_UINT64: out->uint64 = *(const UA_UInt64 *)data; break;
    case UA_DATATYPEKIND_FLOAT: out->float_ = *(const UA_Float *)data; break;
    case UA_DATATYPEKIND_DOUBLE: out->double_ = *(const UA_Double *)data; break;
    default: out->type = type; break;
    }
    return UA_STATUSCODE_GOOD;
}

/* The services that `shim_client_service` sends, by the index of their request and
 * answer types in `UA_TYPES`. `src/ffi.rs` mirrors the order. */
static const struct {
    UA_UInt16 request;
    UA_UInt16 answer;
} SERVICES[] = {
    {UA_TYPES_BROWSEREQUEST, UA_TYPES_BROWSERESPONSE},
    {UA_TYPES_BROWSENEXTREQUEST, UA_TYPES_BROWSENEXTRESPONSE},
    {UA_TYPES_TRANSLATEBROWSEPATHSTONODEIDSREQUEST,
     UA_TYPES_TRANSLATEBROWSEPATHSTONODEIDSRESPONSE},
    {UA_TYPES_READREQUEST, UA_TYPES_READRESPONSE},
};

/* `shim_browse_*` read a BrowseNext answer as a Browse answer. */
_Static_assert(sizeof(UA_BrowseResponse) == sizeof(UA_BrowseNextResponse),
               "UA_BrowseNextResponse changed");
_Static_assert(offsetof(UA_BrowseResponse, results) ==
                   offsetof(UA_BrowseNextResponse, results),
               "UA_BrowseNextResponse.results moved");

/* The request type of `service`, an index of `SERVICES`. */
static const UA_DataType *request_type(size_t service) {
    return &UA_TYPES[SERVICES[service].request];
}

/* The answer type of `service`, an index of `SERVICES`. */
static const UA_DataType *answer_type(size_t service) {
    return &UA_TYPES[SERVICES[service].answer];
}

/* Sends `request`, a request of `service`, on `client` as `send` does, with `callback`
 * for the answer. */
UA_StatusCode shim_client_service(UA_Client *client, void *request, size_t service,
                                  UA_ClientAsyncServiceCallback callback, void *data,
                                  UA_UInt32 *id) {
    return send(client, request, request_type(service), callback, answer_type(service),
                data, id);
}

/* Gives the request type of `service` when not `answer`, else its answer type, for the
 * test of the order of `SERVICES`. */
const UA_DataType *shim_service_type(size_t service, UA_Boolean answer) {
    return answer ? answer_type(service) : request_type(service);
}

/* Moves `answer`, an answer of `service` that a callback gets, to new memory that
 * `shim_answer_free` frees, and leaves `answer` empty for open62541 to clear. Gives
 * NULL, with `answer` as it was, when an allocation fails. */
void *shim_answer_move(void *answer, size_t service) {
    const UA_DataType *type = answer_type(service);
    void *moved = UA_malloc(type->memSize);
    if(!moved)
        return NULL;
    memcpy(moved, answer, type->memSize);
    UA_init(answer, type);
    return moved;
}

/* Frees `answer`, an answer of `service` from `shim_answer_move`. */
void shim_answer_free(void *answer, size_t service) {
    UA_delete(answer, answer_type(service));
}

/* `shim_answer_count` reads the count of results of each answer of `SERVICES` at one
 * place. */
_Static_assert(offsetof(UA_BrowseResponse, resultsSize) ==
                   offsetof(UA_BrowseNextResponse, resultsSize),
               "UA_BrowseNextResponse changed");
_Static_assert(offsetof(UA_BrowseResponse, resultsSize) ==
                   offsetof(UA_TranslateBrowsePathsToNodeIdsResponse, resultsSize),
               "UA_TranslateBrowsePathsToNodeIdsResponse changed");
_Static_assert(offsetof(UA_BrowseResponse, resultsSize) ==
                   offsetof(UA_ReadResponse, resultsSize),
               "UA_ReadResponse changed");

/* Gives the count of results of `answer`, an answer of a service of `SERVICES`. */
size_t shim_answer_count(const void *answer) {
    return ((const UA_BrowseResponse *)answer)->resultsSize;
}

/* Frees `request`, a request of `service`. */
void shim_request_free(void *request, size_t service) {
    UA_delete(request, request_type(service));
}

/* Gives the bytes of the body of the message of `request`, a request of `service`,
 * sent with a session token of `token` encoded bytes. */
size_t shim_request_bytes(const void *request, size_t service, size_t token) {
    const UA_DataType *type = request_type(service);
    const UA_RequestHeader *header = (const UA_RequestHeader *)request;
    const UA_DataType *node = &UA_TYPES[UA_TYPES_NODEID];
    return UA_calcSizeBinary(&type->binaryEncodingId, node, NULL) +
           UA_calcSizeBinary(request, type, NULL) -
           UA_calcSizeBinary(&header->authenticationToken, node, NULL) + token;
}

/* Gives the bytes of the body of the message of a good answer of `service` with no
 * results. Each result adds its `shim_item_*`. */
size_t shim_answer_bytes(size_t service) {
    const UA_DataType *type = answer_type(service);
    /* The largest answer of `SERVICES`, so `UA_init` writes in bounds. */
    UA_TranslateBrowsePathsToNodeIdsResponse answer;
    UA_init(&answer, type);
    return UA_calcSizeBinary(&type->binaryEncodingId, &UA_TYPES[UA_TYPES_NODEID], NULL) +
           UA_calcSizeBinary(&answer, type, NULL);
}

_Static_assert(sizeof(UA_TranslateBrowsePathsToNodeIdsResponse) >=
                       sizeof(UA_BrowseResponse) &&
                   sizeof(UA_TranslateBrowsePathsToNodeIdsResponse) >=
                       sizeof(UA_BrowseNextResponse) &&
                   sizeof(UA_TranslateBrowsePathsToNodeIdsResponse) >=
                       sizeof(UA_ReadResponse),
               "a response of SERVICES grew");

/* The most bytes of each text and byte string of the answers that `shim_item_*`
 * size. */
#define TEXT_BYTES 256

static const UA_Byte TEXT[TEXT_BYTES] = {0};

/* A text of `TEXT_BYTES` bytes, which only a size reads. */
static UA_String text(void) {
    return (UA_String){TEXT_BYTES, (UA_Byte *)(uintptr_t)TEXT};
}

/* Sets `id` to the largest `ExpandedNodeId` with each text at `TEXT_BYTES`. */
static void wide_id(UA_ExpandedNodeId *id) {
    UA_ExpandedNodeId_init(id);
    id->nodeId = UA_NODEID_STRING(UINT16_MAX, "");
    id->nodeId.identifier.string = text();
    id->namespaceUri = text();
    id->serverIndex = UINT32_MAX;
}

/* Gives the bytes of `value` in a `DataValue` with each field. */
static size_t value_bytes(void *value, const UA_DataType *type) {
    UA_DataValue result;
    UA_DataValue_init(&result);
    UA_Variant_setScalar(&result.value, value, type);
    result.hasValue = true;
    result.hasStatus = true;
    result.hasSourceTimestamp = true;
    result.hasServerTimestamp = true;
    result.hasSourcePicoseconds = true;
    result.hasServerPicoseconds = true;
    return UA_calcSizeBinary(&result, &UA_TYPES[UA_TYPES_DATAVALUE], NULL);
}

/* The functions `shim_item_*` each give the most bytes of one result of a good
 * answer, with each text and byte string at `TEXT_BYTES`. */

/* A Read result of a Double, the largest scalar of `shim_value_scalar`. */
size_t shim_item_value(void) {
    UA_Double value = 0;
    return value_bytes(&value, &UA_TYPES[UA_TYPES_DOUBLE]);
}

/* A Read result of a `NodeId` by a name. */
size_t shim_item_fact(void) {
    UA_NodeId id = UA_NODEID_STRING(UINT16_MAX, "");
    id.identifier.string = text();
    return value_bytes(&id, &UA_TYPES[UA_TYPES_NODEID]);
}

/* A Read result of an `EUInformation`. */
size_t shim_item_unit(void) {
    UA_EUInformation unit;
    UA_EUInformation_init(&unit);
    unit.namespaceUri = text();
    unit.displayName = (UA_LocalizedText){text(), text()};
    unit.description = (UA_LocalizedText){text(), text()};
    return value_bytes(&unit, &UA_TYPES[UA_TYPES_EUINFORMATION]);
}

/* A Translate result of one target. */
size_t shim_item_path(void) {
    UA_BrowsePathTarget target;
    wide_id(&target.targetId);
    target.remainingPathIndex = UA_UINT32_MAX;
    UA_BrowsePathResult result = {UA_STATUSCODE_GOOD, 1, &target};
    return UA_calcSizeBinary(&result, &UA_TYPES[UA_TYPES_BROWSEPATHRESULT], NULL);
}

/* A Browse or BrowseNext result with a continuation point and no references. */
size_t shim_item_result(void) {
    UA_BrowseResult result;
    UA_BrowseResult_init(&result);
    result.continuationPoint = text();
    return UA_calcSizeBinary(&result, &UA_TYPES[UA_TYPES_BROWSERESULT], NULL);
}

/* One reference of a Browse result, with each field of the result mask of
 * `shim_browse_new`. */
size_t shim_item_reference(void) {
    UA_ReferenceDescription reference;
    UA_ReferenceDescription_init(&reference);
    reference.referenceTypeId = UA_NODEID_STRING(UINT16_MAX, "");
    reference.referenceTypeId.identifier.string = text();
    wide_id(&reference.nodeId);
    reference.browseName.name = text();
    reference.displayName = (UA_LocalizedText){text(), text()};
    return UA_calcSizeBinary(&reference, &UA_TYPES[UA_TYPES_REFERENCEDESCRIPTION],
                             NULL);
}

/* Gives a Browse request of the references of each of the `count` nodes at `nodes`,
 * which copies their names: the inverse references when `inverse`, else the forward,
 * of the type `reference` and its subtypes, to the node classes of the mask `classes`,
 * at most `most` per node, each with its reference type, node class, browse name, and
 * display name. NULL when an allocation fails. */
UA_BrowseRequest *shim_browse_new(const shim_node *nodes, size_t count,
                                  UA_UInt32 reference, UA_Boolean inverse,
                                  UA_UInt32 classes, UA_UInt32 most) {
    UA_BrowseRequest *request = UA_BrowseRequest_new();
    if(!request)
        return NULL;
    request->requestedMaxReferencesPerNode = most;
    request->nodesToBrowse = (UA_BrowseDescription *)UA_Array_new(
        count, &UA_TYPES[UA_TYPES_BROWSEDESCRIPTION]);
    if(!request->nodesToBrowse) {
        UA_BrowseRequest_delete(request);
        return NULL;
    }
    request->nodesToBrowseSize = count;
    for(size_t i = 0; i < count; i++) {
        UA_BrowseDescription *browse = &request->nodesToBrowse[i];
        browse->browseDirection =
            inverse ? UA_BROWSEDIRECTION_INVERSE : UA_BROWSEDIRECTION_FORWARD;
        browse->referenceTypeId = UA_NODEID_NUMERIC(0, reference);
        browse->includeSubtypes = true;
        browse->nodeClassMask = classes;
        browse->resultMask =
            UA_BROWSERESULTMASK_REFERENCETYPEID | UA_BROWSERESULTMASK_NODECLASS |
            UA_BROWSERESULTMASK_BROWSENAME | UA_BROWSERESULTMASK_DISPLAYNAME;
        UA_NodeId id = shim_node_id(nodes[i]);
        if(UA_NodeId_copy(&id, &browse->nodeId) != UA_STATUSCODE_GOOD) {
            UA_BrowseRequest_delete(request);
            return NULL;
        }
    }
    return request;
}

/* Gives a BrowseNext request of each of the `count` continuation points at `points`,
 * which it copies, that releases them when `release`. NULL when an allocation fails. */
UA_BrowseNextRequest *shim_browse_next_new(const UA_ByteString *points, size_t count,
                                           UA_Boolean release) {
    UA_BrowseNextRequest *request = UA_BrowseNextRequest_new();
    if(!request)
        return NULL;
    request->releaseContinuationPoints = release;
    if(UA_Array_copy(points, count, (void **)&request->continuationPoints,
                     &UA_TYPES[UA_TYPES_BYTESTRING]) != UA_STATUSCODE_GOOD) {
        UA_BrowseNextRequest_delete(request);
        return NULL;
    }
    request->continuationPointsSize = count;
    return request;
}

/* Gives the status of result `index` of `answer`, a Browse or BrowseNext answer with
 * more results than `index`, and writes the count of its references to `references`
 * and its continuation point, which borrows from `answer`, to `point`. */
UA_StatusCode shim_browse_result(const UA_BrowseResponse *answer, size_t index,
                                 size_t *references, UA_ByteString *point) {
    const UA_BrowseResult *result = &answer->results[index];
    *references = result->referencesSize;
    *point = result->continuationPoint;
    return result->statusCode;
}

/* Writes `id` to `out`, which borrows its name from `id`, and gives whether `id` is by
 * a number or a name. Else `out` is `i=0`. */
static UA_Boolean id_of(const UA_NodeId *id, shim_node *out) {
    out->namespace = id->namespaceIndex;
    out->number = 0;
    out->name = NULL;
    out->length = 0;
    if(id->identifierType == UA_NODEIDTYPE_NUMERIC) {
        out->number = id->identifier.numeric;
        return true;
    }
    if(id->identifierType != UA_NODEIDTYPE_STRING)
        return false;
    /* A name is never NULL in a `shim_node`, also an empty one. */
    out->name = id->identifier.string.data ? id->identifier.string.data
                                           : (const UA_Byte *)"";
    out->length = id->identifier.string.length;
    return true;
}

/* A node of this server by a number or a name. */
#define SHIM_HELD_NODE 0
/* A node of this server by a GUID or opaque key. */
#define SHIM_HELD_KEY 1
/* A node of another server, or of a namespace by its URI. */
#define SHIM_HELD_REMOTE 2
_Static_assert(SHIM_HELD_NODE == 0 && SHIM_HELD_KEY == 1 && SHIM_HELD_REMOTE == 2,
               "ffi::held changed");

/* A reference of a browse, as `ffi::Reference` gives it. Each text borrows from the
 * answer. */
typedef struct {
    /* The node, valid only when `held` is 0. */
    shim_node node;
    /* The reference type, valid only when `typed`. */
    shim_node via;
    UA_String name;
    UA_String display;
    UA_UInt32 class;
    /* One of the `SHIM_HELD_*` values. */
    UA_Byte held;
    /* Whether the reference type is by a number or a name. */
    UA_Boolean typed;
} shim_reference;

_Static_assert(sizeof(shim_reference) == 88, "shim_reference changed");
_Static_assert(offsetof(shim_reference, class) == 80, "shim_reference.class moved");
_Static_assert(offsetof(shim_reference, held) == 84, "shim_reference.held moved");
_Static_assert(offsetof(shim_reference, typed) == 85, "shim_reference.typed moved");

/* Writes reference `index` of result `result` of `answer`, a Browse or BrowseNext
 * answer with more of each, to `out`, which borrows from `answer`. */
void shim_browse_reference(const UA_BrowseResponse *answer, size_t result, size_t index,
                           shim_reference *out) {
    const UA_ReferenceDescription *reference =
        &answer->results[result].references[index];
    const UA_ExpandedNodeId *node = &reference->nodeId;
    UA_Boolean keyed = id_of(&node->nodeId, &out->node);
    if(node->serverIndex != 0 || node->namespaceUri.length != 0) {
        out->held = SHIM_HELD_REMOTE;
    } else {
        out->held = keyed ? SHIM_HELD_NODE : SHIM_HELD_KEY;
    }
    out->typed = id_of(&reference->referenceTypeId, &out->via);
    out->name = reference->browseName.name;
    out->display = reference->displayName.text;
    out->class = (UA_UInt32)reference->nodeClass;
}

/* Writes the text of the node of reference `index` of result `result` of `answer`, as
 * `shim_browse_reference` reads it, to `out`, which the caller frees with
 * `shim_buffer_free`. Gives `BadOutOfMemory` when an allocation fails. */
UA_StatusCode shim_browse_text(const UA_BrowseResponse *answer, size_t result,
                               size_t index, UA_String *out) {
    const UA_ReferenceDescription *reference =
        &answer->results[result].references[index];
    UA_String_init(out);
    return UA_ExpandedNodeId_print(&reference->nodeId, out);
}

/* Gives a TranslateBrowsePathsToNodeIds request of the path `HasProperty` to
 * `0:EngineeringUnits` from each of the `count` nodes at `nodes`, which copies their
 * names, or NULL when an allocation fails. */
UA_TranslateBrowsePathsToNodeIdsRequest *shim_units_new(const shim_node *nodes,
                                                        size_t count) {
    UA_TranslateBrowsePathsToNodeIdsRequest *request =
        UA_TranslateBrowsePathsToNodeIdsRequest_new();
    if(!request)
        return NULL;
    request->browsePaths =
        (UA_BrowsePath *)UA_Array_new(count, &UA_TYPES[UA_TYPES_BROWSEPATH]);
    if(!request->browsePaths) {
        UA_TranslateBrowsePathsToNodeIdsRequest_delete(request);
        return NULL;
    }
    request->browsePathsSize = count;
    for(size_t i = 0; i < count; i++) {
        UA_BrowsePath *path = &request->browsePaths[i];
        UA_RelativePathElement step;
        UA_RelativePathElement_init(&step);
        step.referenceTypeId = UA_NODEID_NUMERIC(0, UA_NS0ID_HASPROPERTY);
        step.targetName = UA_QUALIFIEDNAME(0, "EngineeringUnits");
        UA_NodeId id = shim_node_id(nodes[i]);
        UA_RelativePath relative = {1, &step};
        if(UA_NodeId_copy(&id, &path->startingNode) != UA_STATUSCODE_GOOD ||
           UA_RelativePath_copy(&relative, &path->relativePath) != UA_STATUSCODE_GOOD) {
            UA_TranslateBrowsePathsToNodeIdsRequest_delete(request);
            return NULL;
        }
    }
    return request;
}

/* Writes the node of the first target of result `index` of `answer`, a
 * TranslateBrowsePathsToNodeIds answer with more results than `index`, to `out`, which
 * borrows its name from `answer`. Gives false when the result is not Good, or its
 * first target is not in this server by a number or a name, or ends before the path
 * does. */
UA_Boolean shim_units_target(const UA_TranslateBrowsePathsToNodeIdsResponse *answer,
                             size_t index, shim_node *out) {
    const UA_BrowsePathResult *result = &answer->results[index];
    if(result->statusCode != UA_STATUSCODE_GOOD || result->targetsSize == 0)
        return false;
    const UA_BrowsePathTarget *target = &result->targets[0];
    return target->remainingPathIndex == UA_UINT32_MAX &&
           target->targetId.serverIndex == 0 &&
           target->targetId.namespaceUri.length == 0 &&
           id_of(&target->targetId.nodeId, out);
}

/* Gives a Read request of the attribute `attributes[i]` of the node `nodes[i]` for each
 * `i` below `count`, as `read_new` does. */
UA_ReadRequest *shim_read_attributes_new(const shim_node *nodes,
                                         const UA_UInt32 *attributes, size_t count) {
    return read_new(nodes, attributes, count);
}

/* Writes the node of `value`, an answer of a read that holds a scalar `NodeId` by a
 * number or a name, to `out`, which borrows its name from `value`. Else gives the
 * status of `shim_value_status` when it is not Good, else `BadTypeMismatch`. */
UA_StatusCode shim_value_node(const UA_DataValue *value, shim_node *out) {
    UA_StatusCode status = shim_value_status(value);
    if(status != UA_STATUSCODE_GOOD)
        return status;
    if(!UA_Variant_hasScalarType(&value->value, &UA_TYPES[UA_TYPES_NODEID]))
        return UA_STATUSCODE_BADTYPEMISMATCH;
    return id_of((const UA_NodeId *)value->value.data, out)
               ? UA_STATUSCODE_GOOD
               : UA_STATUSCODE_BADTYPEMISMATCH;
}

/* Writes the `namespaceUri` of `value`, an answer of a read that holds a scalar
 * `EUInformation`, to `uri`, its `unitId` to `code`, and the text of its display name
 * to `text`. Each text borrows from `value`. Else gives the status of
 * `shim_value_status` when it is not Good, else `BadTypeMismatch`. */
UA_StatusCode shim_value_unit(const UA_DataValue *value, UA_String *uri,
                              UA_Int32 *code, UA_String *text) {
    UA_StatusCode status = shim_value_status(value);
    if(status != UA_STATUSCODE_GOOD)
        return status;
    if(!UA_Variant_hasScalarType(&value->value, &UA_TYPES[UA_TYPES_EUINFORMATION]))
        return UA_STATUSCODE_BADTYPEMISMATCH;
    const UA_EUInformation *unit = (const UA_EUInformation *)value->value.data;
    *uri = unit->namespaceUri;
    *code = unit->unitId;
    *text = unit->displayName.text;
    return UA_STATUSCODE_GOOD;
}
