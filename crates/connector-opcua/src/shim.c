/* The symbols that the copy leaves undefined when it is built with no architecture, the
   event loop that `src/event.rs` gives the copy, and the connection manager that
   `src/connection.rs` gives it. */

/* The headers of the copy have unused parameters. Any other warning in them fails the
   build. */
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wunused-parameter"
#include <open62541/client_config_default.h>
#include <open62541/plugin/eventloop.h>
#include <open62541/types.h>
#include <open62541/util.h>
#include "mp_printf.h"
#include "timer.h"
#pragma GCC diagnostic pop

#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* The global clocks give a fixed time, so no OS clock enters through the C code.
 * `cargo xtask open62541` lists each call site. */
UA_DateTime UA_DateTime_now(void) { return 0; }
UA_DateTime UA_DateTime_nowMonotonic(void) { return 0; }
UA_Int64 UA_DateTime_localTimeUtcOffset(void) { return 0; }

/* The library calls these only when a config has no event loop, and ours always has
 * one, so a call is a defect. */
static void refuse(const char *name) {
    fprintf(stderr, "connector-opcua: open62541 called %s, which is not built\n", name);
    abort();
}

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

/* `src/ffi.rs` mirrors these and asserts the same sizes. */
_Static_assert(sizeof(UA_NodeId) == 24, "UA_NodeId changed");
_Static_assert(sizeof(UA_DataType) == 96, "UA_DataType changed");
_Static_assert(sizeof(UA_DecodeBinaryOptions) == 40, "UA_DecodeBinaryOptions changed");
_Static_assert(offsetof(UA_DecodeBinaryOptions, decodedLength) == 32,
               "decodedLength moved");
_Static_assert(UA_TYPES_COUNT == 388, "UA_TYPES changed");
_Static_assert(UA_TYPES_BYTESTRING == 14, "UA_TYPES_BYTESTRING moved");
_Static_assert(UA_TYPES_VARIANT == 23, "UA_TYPES_VARIANT moved");

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

/* Gives the monotonic time of `clock` in ticks of 100 ns, at most `LAST_TICK`. */
typedef UA_DateTime (*shim_now)(void *clock);

/* An event loop with no I/O. It runs on one thread and never blocks. */
struct shim_loop {
    /* First, so a pointer to it is a pointer to the loop. */
    UA_EventLoop el;
    UA_Timer timer;
    UA_Logger logger;
    shim_now now;
    void *clock;
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
    return loop->now(loop->clock);
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
    return now_of(el) + UA_DATETIME_UNIX_EPOCH;
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

/* The bytes of a log line, with its newline. */
#define LOG_BYTES 512

/* Writes each message of level warning and up to fd 2, in one `write`, so a line takes
 * no `stdio` lock and does not mix with a line of another thread. A line longer than
 * `LOG_BYTES` is cut. It allocates nothing. */
static void log_message(void *context, UA_LogLevel level, UA_LogCategory category,
                        const char *msg, va_list args) {
    (void)context;
    (void)category;
    if(level < UA_LOGLEVEL_WARNING)
        return;
    char line[LOG_BYTES];
    /* `mp_vsnprintf`, not `vsnprintf`: open62541 formats `%S` and `%N`. */
    int prefix = mp_snprintf(line, sizeof(line), "connector-opcua: open62541 %s: ",
                             LEVELS[level / 100 - 1]);
    int text = mp_vsnprintf(line + prefix, sizeof(line) - (size_t)prefix, msg, args);
    size_t length = (size_t)prefix + (size_t)text;
    if(length > LOG_BYTES - 1)
        length = LOG_BYTES - 1;
    line[length] = '\n';
    /* A failed write of a log line has nowhere to go. */
    ssize_t written = write(STDERR_FILENO, line, length + 1);
    (void)written;
}

/* Logs the `length` bytes at `message` as a warning through the logger of `el`.
 * `length` is above 0: mp_printf reads a precision of 0 as none, and reads to a NUL. */
void shim_log_warning(UA_EventLoop *el, const char *message, size_t length) {
    int bytes = length < LOG_BYTES ? (int)length : LOG_BYTES;
    UA_LOG_WARNING(el->logger, UA_LOGCATEGORY_NETWORK, "%.*s", bytes, message);
}

/* Gives a fresh loop whose time is `now(clock)`, or NULL when out of memory. */
UA_EventLoop *shim_loop_new(shim_now now, void *clock) {
    struct shim_loop *loop = (struct shim_loop *)UA_calloc(1, sizeof(*loop));
    if(!loop)
        return NULL;
    UA_Timer_init(&loop->timer);
    loop->now = now;
    loop->clock = clock;
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

/* The most passes of delayed callbacks that a free runs. A callback that queues
 * itself at each pass is a defect. */
#define FREE_PASSES 64

/* Runs the queued delayed callbacks, which free what they hold, and those that they
 * queue, removes each timer, and frees `el`, a loop of `shim_loop_new`. Aborts when
 * callbacks are still queued after `FREE_PASSES` passes. */
void shim_loop_free(UA_EventLoop *el) {
    struct shim_loop *loop = loop_of(el);
    for(int pass = 0; loop->queued; pass++) {
        if(pass == FREE_PASSES) {
            fprintf(stderr, "connector-opcua: open62541 queued delayed callbacks for "
                            "%d passes of a loop free\n", FREE_PASSES);
            abort();
        }
        run_delayed(loop);
    }
    UA_Timer_clear(&loop->timer);
    UA_free(loop);
}

/* The hooks of `src/connection.rs`. Each takes the `state` of `shim_cm_new`. */
typedef struct {
    UA_StatusCode (*open)(void *state, UA_String host, UA_UInt16 port, void *application,
                          void *context, UA_ConnectionManager_connectionCallback callback);
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

/* `src/ffi.rs` mirrors the struct for the tests, and asserts the same offsets. */
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

/* Opens a client connection to the `address` and `port` of `params`. */
static UA_StatusCode cm_open(UA_ConnectionManager *cm, const UA_KeyValueMap *params,
                             void *application, void *context,
                             UA_ConnectionManager_connectionCallback callback) {
    const UA_Boolean *listen = (const UA_Boolean *)UA_KeyValueMap_getScalar(
        params, UA_QUALIFIEDNAME(0, "listen"), &UA_TYPES[UA_TYPES_BOOLEAN]);
    if(listen && *listen)
        return UA_STATUSCODE_BADNOTSUPPORTED;
    const UA_String *address = (const UA_String *)UA_KeyValueMap_getScalar(
        params, UA_QUALIFIEDNAME(0, "address"), &UA_TYPES[UA_TYPES_STRING]);
    const UA_UInt16 *port = (const UA_UInt16 *)UA_KeyValueMap_getScalar(
        params, UA_QUALIFIEDNAME(0, "port"), &UA_TYPES[UA_TYPES_UINT16]);
    if(!address || !port)
        return UA_STATUSCODE_BADINVALIDARGUMENT;
    struct shim_cm *s = cm_of(cm);
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

/* Gives a client on `el`, or NULL on a failure. */
UA_Client *shim_client_new(UA_EventLoop *el) {
    UA_ClientConfig config;
    memset(&config, 0, sizeof(config));
    config.logging = &loop_of(el)->logger;
    config.eventLoop = el;
    config.externalEventLoop = true;
    if(UA_ClientConfig_setDefault(&config) != UA_STATUSCODE_GOOD) {
        UA_ClientConfig_clear(&config);
        return NULL;
    }
    return UA_Client_newWithConfig(&config);
}
