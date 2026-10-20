/* The test server, which only a build with the feature `sim` compiles. No other C
   file of `src/` names a `UA_Server`, so a production build cannot reach its
   `setDefaultConfig`. */

/* The headers of the copy have unused parameters. Any other warning in them fails the
   build. */
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wunused-parameter"
#include <open62541/client.h>
#include <open62541/plugin/certificategroup_default.h>
#include <open62541/server_config_default.h>
/* For the continuation points that each session holds. */
#include "client/ua_client_internal.h"
#include "server/ua_server_internal.h"
#pragma GCC diagnostic pop

#include <string.h>

#include "shim.h"

/* Gives `server` the policies of `security`, with an endpoint in Sign and in
 * SignAndEncrypt for each of the `count` policy URIs of `policies`, in that order, and a
 * channel check that trusts only the certificates that `security` trusts. Gives
 * `BadSecurityPolicyRejected` for a URI that the copy does not compile. */
static UA_StatusCode secure_server(UA_ServerConfig *config,
                                   const shim_security *security,
                                   const char *const *policies, size_t count) {
    size_t first = config->securityPoliciesSize;
    UA_StatusCode status = shim_add_policies(&config->securityPolicies,
                                             &config->securityPoliciesSize, security,
                                             config->logging);
    for(size_t i = 0; status == UA_STATUSCODE_GOOD && i < count; i++) {
        UA_String uri = UA_STRING((char *)(uintptr_t)policies[i]);
        size_t at = first;
        while(at < config->securityPoliciesSize &&
              !UA_String_equal(&config->securityPolicies[at].policyUri, &uri))
            at++;
        if(at == config->securityPoliciesSize)
            return UA_STATUSCODE_BADSECURITYPOLICYREJECTED;
        status = UA_ServerConfig_addEndpoint(config, uri, UA_MESSAGESECURITYMODE_SIGN);
        if(status == UA_STATUSCODE_GOOD)
            status = UA_ServerConfig_addEndpoint(config, uri,
                                                 UA_MESSAGESECURITYMODE_SIGNANDENCRYPT);
    }
    if(status != UA_STATUSCODE_GOOD)
        return status;
    /* `UA_ServerConfig_setMinimal` gives the group the accept-all group. */
    config->secureChannelPKI.clear(&config->secureChannelPKI);
    return shim_trust(&config->secureChannelPKI, &security->trust, config->logging,
                      config->eventLoop);
}

/* Gives a server on `el` with the minimal config, the one server URL
 * `opc.tcp://{host}:{port}`, room for `sessions` sessions and secure channels, and at
 * most `chunks` chunks in a message, or NULL on a failure. With `security`, the server
 * also has an endpoint in Sign and in SignAndEncrypt for each of the `count` policy
 * URIs of `policies`, in that order, and its channels trust only the certificates that
 * `security` trusts. */
UA_Server *shim_server_new(UA_EventLoop *el, UA_UInt16 port, const char *host,
                           UA_UInt16 sessions, UA_UInt32 chunks,
                           const shim_security *security,
                           const char *const *policies, size_t count) {
    UA_ServerConfig config;
    memset(&config, 0, sizeof(config));
    /* The loop of `shim_loop_new` owns its logger, which is not const. */
    config.logging = (UA_Logger *)(uintptr_t)el->logger;
    config.eventLoop = el;
    config.externalEventLoop = true;
    if(UA_ServerConfig_setMinimal(&config, port, NULL) != UA_STATUSCODE_GOOD ||
       (security && secure_server(&config, security, policies, count) != UA_STATUSCODE_GOOD)) {
        UA_ServerConfig_clear(&config);
        return NULL;
    }
    config.maxSessions = sessions;
    config.maxSecureChannels = sessions;
    config.tcpMaxChunks = chunks;
    /* The minimal config has the one server URL `opc.tcp://:{port}`. */
    UA_String *url = &config.serverUrls[0];
    UA_String_clear(url);
    if(UA_String_format(url, "opc.tcp://%s:%u", host, (unsigned)port) !=
       UA_STATUSCODE_GOOD) {
        UA_ServerConfig_clear(&config);
        return NULL;
    }
    return UA_Server_newWithConfig(&config);
}

/* Gives the application URI that `client` sends, which lives as long as its config. */
const UA_String *shim_client_uri(UA_Client *client) {
    return &UA_Client_getConfig(client)->clientDescription.applicationUri;
}

/* Gives the policy URI of the channel of `client`, which lives as long as the client
 * config, and its mode in `mode`, or NULL with no channel. */
const UA_String *shim_client_channel(UA_Client *client, UA_MessageSecurityMode *mode) {
    UA_Variant policy, held;
    if(UA_Client_getConnectionAttribute(client, UA_QUALIFIEDNAME(0, "securityPolicyUri"),
                                        &policy) != UA_STATUSCODE_GOOD ||
       UA_Client_getConnectionAttribute(client, UA_QUALIFIEDNAME(0, "securityMode"),
                                        &held) != UA_STATUSCODE_GOOD)
        return NULL;
    *mode = *(const UA_MessageSecurityMode *)held.data;
    return (const UA_String *)policy.data;
}

/* Gives discovery URL `index` of `server`, or NULL past the last. */
const UA_String *shim_server_discovery_url(UA_Server *server, size_t index) {
    const UA_ApplicationDescription *description =
        &UA_Server_getConfig(server)->applicationDescription;
    if(index >= description->discoveryUrlsSize)
        return NULL;
    return &description->discoveryUrls[index];
}

/* Sets the buffer size, the MaxMessageSize, and the MaxChunkCount of each later channel
 * of `server`, each 0 for the default of open62541. */
void shim_server_limits(UA_Server *server, UA_UInt32 buffer, UA_UInt32 message,
                        UA_UInt32 chunks) {
    UA_ServerConfig *config = UA_Server_getConfig(server);
    config->tcpBufSize = buffer;
    config->tcpMaxMsgSize = message;
    config->tcpMaxChunks = chunks;
}

/* Sets the MaxNodesPerRead of `server`, 0 for none. */
void shim_server_reads(UA_Server *server, UA_UInt32 most) {
    UA_Server_getConfig(server)->maxNodesPerRead = most;
}

/* Sets the most references of each page of a Browse of `server`. */
void shim_server_references(UA_Server *server, UA_UInt32 most) {
    UA_Server_getConfig(server)->maxReferencesPerNode = most;
}

/* Sets the MaxNodesPerBrowse and the MaxNodesPerTranslateBrowsePathsToNodeIds of
 * `server`, each 0 for none. */
void shim_server_browses(UA_Server *server, UA_UInt32 browse, UA_UInt32 translate) {
    UA_ServerConfig *config = UA_Server_getConfig(server);
    config->maxNodesPerBrowse = browse;
    config->maxNodesPerTranslateBrowsePathsToNodeIds = translate;
}

/* Sets the MaxMonitoredItemsPerCall of `server`, 0 for none. */
void shim_server_monitors(UA_Server *server, UA_UInt32 most) {
    UA_Server_getConfig(server)->maxMonitoredItemsPerCall = most;
}

/* Ends each subscription of `server` with `status`, which the server sends in a status
 * change notification at the next publishing interval of each. */
void shim_server_end(UA_Server *server, UA_StatusCode status) {
    UA_Subscription *sub;
    LIST_FOREACH(sub, &server->subscriptions, serverListEntry)
        sub->statusChange = status;
}

/* Sets the most notifications of a Publish answer that `server` grants. */
void shim_server_publishes(UA_Server *server, UA_UInt32 most) {
    UA_Server_getConfig(server)->maxNotificationsPerPublish = most;
}

/* Sets the most Publish requests that `server` holds for each session. */
void shim_server_holds(UA_Server *server, UA_UInt32 most) {
    UA_Server_getConfig(server)->maxPublishReqPerSession = most;
}

/* Sets the shortest publishing interval that `server` grants, in ms. */
void shim_server_publishing(UA_Server *server, UA_Double ms) {
    UA_Server_getConfig(server)->publishingIntervalLimits.min = ms;
}

/* Sets the smallest keep-alive count of a subscription that `server` grants. */
void shim_server_keep_alives(UA_Server *server, UA_UInt32 least) {
    UA_Server_getConfig(server)->keepAliveCountLimits.min = least;
}

/* Gives the most Publish requests that one session of `server` holds now. */
UA_UInt32 shim_server_held(UA_Server *server) {
    size_t most = 0;
    session_list_entry *entry;
    LIST_FOREACH(entry, &server->sessions, pointers) {
        if(entry->session.responseQueueSize > most)
            most = entry->session.responseQueueSize;
    }
    return (UA_UInt32)most;
}

/* Sets the largest queue of a monitored item that `server` grants. */
void shim_server_queues(UA_Server *server, UA_UInt32 most) {
    UA_Server_getConfig(server)->queueSizeLimits.max = most;
}

/* Gives the most notifications of a Publish answer of the first subscription of
 * `server`, 0 for none. */
UA_UInt32 shim_server_per_publish(UA_Server *server) {
    UA_Subscription *sub = LIST_FIRST(&server->subscriptions);
    return sub ? sub->notificationsPerPublish : 0;
}

/* The counts of a `shim_counter`. */
typedef struct {
    /* The Browse requests. */
    size_t browses;
    /* The most continuation points that one session held at a message. */
    size_t points;
    /* The CreateMonitoredItems requests. */
    size_t monitors;
    /* The Publish requests. */
    size_t publishes;
    /* The Publish requests answered with BadTooManyPublishRequests. */
    size_t refused;
} shim_counts;

/* Gives whether `msg`, of `length` bytes, ends with `end`. */
static UA_Boolean ends_with(const char *msg, size_t length, const char *end) {
    size_t bytes = strlen(end);
    return length >= bytes && strcmp(msg + length - bytes, end) == 0;
}

/* A logger that counts the Browse, CreateMonitoredItems, and Publish requests of its
 * server, its refusals of Publish requests, and the continuation points of its
 * sessions, and passes each message to the logger before it. */
typedef struct {
    UA_Logger logger;
    const UA_Logger *next;
    UA_Server *server;
    shim_counts counts;
} shim_counter;

static void count_message(void *context, UA_LogLevel level, UA_LogCategory category,
                          const char *msg, va_list args) {
    shim_counter *counter = (shim_counter *)context;
    /* The ends of the formats of the debug messages of open62541 for each request,
     * from UA_LOG_SESSION_INTERNAL. */
    size_t length = strlen(msg);
    if(ends_with(msg, length, "| Processing BrowseRequest%.0s"))
        counter->counts.browses++;
    else if(ends_with(msg, length, "| Processing CreateMonitoredItemsRequest%.0s"))
        counter->counts.monitors++;
    else if(ends_with(msg, length, "| Processing PublishRequest with RequestId %u%.0s"))
        counter->counts.publishes++;
    else if(ends_with(msg, length,
                      "| Sending out a publish response triggered by too many publish "
                      "requests%.0s"))
        counter->counts.refused++;
    session_list_entry *entry;
    LIST_FOREACH(entry, &counter->server->sessions, pointers) {
        size_t held = entry->session.continuationPointsSize;
        if(held > counter->counts.points)
            counter->counts.points = held;
    }
    counter->next->log(counter->next->context, level, category, msg, args);
}

static void count_clear(UA_Logger *logger) {
    shim_counter *counter = (shim_counter *)logger->context;
    const UA_Logger *next = counter->next;
    UA_free(counter);
    /* Each count wraps the count before it. The logger under the first is the logger
     * of the event loop, which the server does not own. */
    if(next->clear == count_clear)
        next->clear((UA_Logger *)(uintptr_t)next);
}

/* Counts each Browse, CreateMonitoredItems, and Publish request that `server`
 * processes from now, and the most continuation points that one of its sessions holds,
 * and gives the counts, which live until the delete of `server`, or NULL when out of
 * memory. */
const shim_counts *shim_server_count(UA_Server *server) {
    shim_counter *counter = (shim_counter *)UA_calloc(1, sizeof(*counter));
    if(!counter)
        return NULL;
    UA_ServerConfig *config = UA_Server_getConfig(server);
    counter->next = config->logging;
    counter->logger.log = count_message;
    counter->logger.context = counter;
    counter->logger.clear = count_clear;
    counter->server = server;
    config->logging = &counter->logger;
    return &counter->counts;
}

/* Logs `text` at level fatal in category `client` through the logger of `el`. No
 * message of the copy that a test reaches is fatal. */
void shim_log_fatal(UA_EventLoop *el, const char *text) {
    UA_LOG_FATAL(el->logger, UA_LOGCATEGORY_CLIENT, "%s", text);
}

/* Logs `text` at level warning in category `client` through the logger of `el`. */
void shim_log_warning(UA_EventLoop *el, const char *text) {
    UA_LOG_WARNING(el->logger, UA_LOGCATEGORY_CLIENT, "%s", text);
}

/* Logs `format` with its arguments at `level` in `category` through `logger`. */
static void log_at(const UA_Logger *logger, UA_LogLevel level,
                   UA_LogCategory category, const char *format, ...) {
    va_list args;
    va_start(args, format);
    logger->log(logger->context, level, category, format, args);
    va_end(args);
}

/* Logs `text` at `level` in `category` through the logger of `el`, also at a value
 * that the enums do not name. */
void shim_log_at(UA_EventLoop *el, int level, int category, const char *text) {
    log_at(el->logger, (UA_LogLevel)level, (UA_LogCategory)category, "%s", text);
}

/* Sets the receive buffer size that `client` gives in its next HEL. */
void shim_client_receive(UA_Client *client, UA_UInt32 size) {
    UA_Client_getConfig(client)->localConnectionConfig.recvBufferSize = size;
}

/* Gives the count of blocks that `client` keeps for the decode of a Publish answer. */
size_t shim_client_blocks(UA_Client *client) {
    size_t count = 0;
    for(PublishBlock *block = client->publishArena.blocks; block; block = block->next)
        count++;
    return count;
}

/* Sets the MaxMessageSize that `client` gives in its next HEL and takes. */
void shim_client_message(UA_Client *client, UA_UInt32 size) {
    UA_Client_getConfig(client)->localConnectionConfig.localMaxMessageSize = size;
}

/* The node context of a variable of `shim_server_add_source`, as the first field of
 * the context that Rust holds. */
typedef struct shim_source shim_source;
struct shim_source {
    /* Gives the value, its kind, the status, and the source timestamp, 0 for none, of
     * a read of the variable. */
    void (*read)(shim_source *source, shim_scalar *value, UA_UInt16 *kind,
                 UA_StatusCode *status, UA_DateTime *time);
};

static UA_StatusCode shim_source_read(UA_Server *server, const UA_NodeId *sessionId,
                                      void *sessionContext, const UA_NodeId *nodeId,
                                      void *nodeContext,
                                      UA_Boolean includeSourceTimeStamp,
                                      const UA_NumericRange *range,
                                      UA_DataValue *value) {
    (void)server;
    (void)sessionId;
    (void)sessionContext;
    (void)nodeId;
    /* The client of the crate reads no range. */
    (void)range;
    value->hasStatus = true;
    shim_source *source = nodeContext;
    shim_scalar read;
    UA_UInt16 kind;
    UA_DateTime time;
    source->read(source, &read, &kind, &value->status, &time);
    /* With no source timestamp, open62541 gives the time of the read. */
    value->hasSourceTimestamp = includeSourceTimeStamp && time != 0;
    value->sourceTimestamp = time;
    UA_StatusCode status =
        UA_Variant_setScalarCopy(&value->value, &read, &UA_TYPES[kind]);
    value->hasValue = status == UA_STATUSCODE_GOOD;
    return status;
}

/* Adds to `server` the scalar variable `node` of the DataType BaseDataType under the
 * Objects folder, whose reads `source` answers. `source` lives until the server is
 * deleted. */
UA_StatusCode shim_server_add_source(UA_Server *server, shim_node node,
                                     shim_source *source) {
    UA_VariableAttributes attributes = UA_VariableAttributes_default;
    attributes.dataType = UA_NODEID_NUMERIC(0, UA_NS0ID_BASEDATATYPE);
    attributes.valueRank = UA_VALUERANK_SCALAR;
    attributes.accessLevel = UA_ACCESSLEVELMASK_READ;
    UA_CallbackValueSource callbacks = {.read = shim_source_read, .write = NULL};
    return UA_Server_addCallbackValueSourceVariableNode(
        server, shim_node_id(node),
        UA_NODEID_NUMERIC(0, UA_NS0ID_OBJECTSFOLDER),
        UA_NODEID_NUMERIC(0, UA_NS0ID_ORGANIZES),
        UA_QUALIFIEDNAME(node.namespace, "source"),
        UA_NODEID_NUMERIC(0, UA_NS0ID_BASEDATAVARIABLETYPE), attributes, callbacks,
        source, NULL);
}

/* Makes `source` answer each later read of the variable `node` of `server`, which
 * open62541 adds, as `shim_server_add_source` does. `source` lives until the server
 * is deleted. */
UA_StatusCode shim_server_set_source(UA_Server *server, shim_node node,
                                     shim_source *source) {
    UA_NodeId id = shim_node_id(node);
    UA_StatusCode status = UA_Server_setNodeContext(server, id, source);
    if(status != UA_STATUSCODE_GOOD)
        return status;
    UA_CallbackValueSource callbacks = {.read = shim_source_read, .write = NULL};
    return UA_Server_setVariableNode_callbackValueSource(server, id, callbacks);
}

/* Sets the most session timeout of `server`, in ms. */
void shim_server_session_timeout(UA_Server *server, UA_UInt32 ms) {
    UA_Server_getConfig(server)->maxSessionTimeout = ms;
}

/* Gives in `id` the number of the DataType of the variable `node` of `server`.
 * Returns `UA_STATUSCODE_BADTYPEMISMATCH` when the DataType is not a number of
 * namespace 0. */
UA_StatusCode shim_server_data_type(UA_Server *server, shim_node node, UA_UInt32 *id) {
    UA_NodeId type;
    UA_StatusCode status = UA_Server_readDataType(server, shim_node_id(node), &type);
    if(status != UA_STATUSCODE_GOOD)
        return status;
    if(type.namespaceIndex != 0 || type.identifierType != UA_NODEIDTYPE_NUMERIC)
        status = UA_STATUSCODE_BADTYPEMISMATCH;
    else
        *id = type.identifier.numeric;
    UA_NodeId_clear(&type);
    return status;
}

/* Gives the text `name` in the namespace of `node`, which borrows `name`. */
static UA_QualifiedName name_of(shim_node node, const char *name) {
    return UA_QUALIFIEDNAME(node.namespace, (char *)(uintptr_t)name);
}

/* Adds to `server` the folder `node` with the browse name `name` and the display name
 * `display`, under `parent` by the reference of the number `reference`. */
UA_StatusCode shim_server_add_folder(UA_Server *server, shim_node node,
                                     shim_node parent, UA_UInt32 reference,
                                     const char *name, const char *display) {
    UA_ObjectAttributes attributes = UA_ObjectAttributes_default;
    attributes.displayName = UA_LOCALIZEDTEXT("", (char *)(uintptr_t)display);
    return UA_Server_addObjectNode(
        server, shim_node_id(node), shim_node_id(parent),
        UA_NODEID_NUMERIC(0, reference), name_of(node, name),
        UA_NODEID_NUMERIC(0, UA_NS0ID_FOLDERTYPE), attributes, NULL, NULL);
}

/* Adds to `server` the folder of the GUID 1-2-3-4 in namespace 1, with the browse name
 * `guid`, under `parent`. */
UA_StatusCode shim_server_add_guid(UA_Server *server, shim_node parent) {
    UA_Guid guid = {1, 2, 3, {4, 0, 0, 0, 0, 0, 0, 0}};
    UA_ObjectAttributes attributes = UA_ObjectAttributes_default;
    attributes.displayName = UA_LOCALIZEDTEXT("", "guid");
    return UA_Server_addObjectNode(
        server, UA_NODEID_GUID(1, guid), shim_node_id(parent),
        UA_NODEID_NUMERIC(0, UA_NS0ID_ORGANIZES), UA_QUALIFIEDNAME(1, "guid"),
        UA_NODEID_NUMERIC(0, UA_NS0ID_FOLDERTYPE), attributes, NULL, NULL);
}

/* Adds to `server` the variable `node` with the browse and display name `name`, the
 * DataType `type`, the ValueRank `rank`, with the ArrayDimensions 0 for the rank 1, and
 * the AccessLevel and UserAccessLevel `access`, under `parent` by the reference of the
 * number `reference`. Its value is the OPC UA binary encoding `encoded` of `size` bytes
 * of a Variant, or none for NULL. */
UA_StatusCode shim_server_add_variable(UA_Server *server, shim_node node,
                                       shim_node parent, UA_UInt32 reference,
                                       const char *name, shim_node type, UA_Int32 rank,
                                       UA_Byte access, const UA_Byte *encoded,
                                       size_t size) {
    UA_VariableAttributes attributes = UA_VariableAttributes_default;
    attributes.displayName = UA_LOCALIZEDTEXT("", (char *)(uintptr_t)name);
    attributes.dataType = shim_node_id(type);
    attributes.valueRank = rank;
    UA_UInt32 dimensions[1] = {0};
    if(rank == 1) {
        attributes.arrayDimensions = dimensions;
        attributes.arrayDimensionsSize = 1;
    }
    attributes.accessLevel = access;
    attributes.userAccessLevel = access;
    if(encoded != NULL) {
        const UA_ByteString bytes = {size, (UA_Byte *)(uintptr_t)encoded};
        UA_StatusCode status = UA_decodeBinary(&bytes, &attributes.value,
                                               &UA_TYPES[UA_TYPES_VARIANT], NULL);
        if(status != UA_STATUSCODE_GOOD)
            return status;
    }
    UA_StatusCode status = UA_Server_addVariableNode(
        server, shim_node_id(node), shim_node_id(parent),
        UA_NODEID_NUMERIC(0, reference), name_of(node, name),
        UA_NODEID_NUMERIC(0, UA_NS0ID_BASEDATAVARIABLETYPE), attributes, NULL, NULL);
    UA_Variant_clear(&attributes.value);
    return status;
}

/* Adds to `server` the data type `node` with the browse name `name`, a subtype of
 * `parent`. */
UA_StatusCode shim_server_add_data_type(UA_Server *server, shim_node node,
                                        shim_node parent, const char *name) {
    UA_DataTypeAttributes attributes = UA_DataTypeAttributes_default;
    attributes.displayName = UA_LOCALIZEDTEXT("", (char *)(uintptr_t)name);
    return UA_Server_addDataTypeNode(server, shim_node_id(node), shim_node_id(parent),
                                     UA_NODEID_NUMERIC(0, UA_NS0ID_HASSUBTYPE),
                                     name_of(node, name), attributes, NULL, NULL);
}

/* Adds to `server` the property `0:EngineeringUnits` `node` of the variable
 * `variable`, whose value is the EUInformation of the namespaceUri `uri`, the display
 * name `text`, and the unitId `code`. */
UA_StatusCode shim_server_add_unit(UA_Server *server, shim_node node,
                                   shim_node variable, const char *uri,
                                   const char *text, UA_Int32 code) {
    UA_EUInformation unit;
    UA_EUInformation_init(&unit);
    unit.namespaceUri = UA_STRING((char *)(uintptr_t)uri);
    unit.unitId = code;
    unit.displayName = UA_LOCALIZEDTEXT("", (char *)(uintptr_t)text);
    UA_VariableAttributes attributes = UA_VariableAttributes_default;
    attributes.displayName = UA_LOCALIZEDTEXT("", "EngineeringUnits");
    attributes.dataType = UA_NODEID_NUMERIC(0, UA_NS0ID_BASEDATATYPE);
    attributes.accessLevel = UA_ACCESSLEVELMASK_READ;
    UA_Variant_setScalar(&attributes.value, &unit, &UA_TYPES[UA_TYPES_EUINFORMATION]);
    return UA_Server_addVariableNode(
        server, shim_node_id(node), shim_node_id(variable),
        UA_NODEID_NUMERIC(0, UA_NS0ID_HASPROPERTY),
        UA_QUALIFIEDNAME(0, "EngineeringUnits"),
        UA_NODEID_NUMERIC(0, UA_NS0ID_PROPERTYTYPE), attributes, NULL, NULL);
}

/* Adds to `server` a reference of the number `reference` from `source` to `target`. */
UA_StatusCode shim_server_add_reference(UA_Server *server, shim_node source,
                                        UA_UInt32 reference, shim_node target) {
    UA_ExpandedNodeId to = UA_EXPANDEDNODEID_NULL;
    to.nodeId = shim_node_id(target);
    return UA_Server_addReference(server, shim_node_id(source),
                                  UA_NODEID_NUMERIC(0, reference), to, true);
}

/* Deletes from `server` the reference of the number `reference` from `source` to
 * `target`, in both directions. */
UA_StatusCode shim_server_delete_reference(UA_Server *server, shim_node source,
                                           UA_UInt32 reference, shim_node target) {
    UA_ExpandedNodeId to = UA_EXPANDEDNODEID_NULL;
    to.nodeId = shim_node_id(target);
    return UA_Server_deleteReference(server, shim_node_id(source),
                                     UA_NODEID_NUMERIC(0, reference), true, to, true);
}

/* Adds to `server` the view `node` with the browse and display name `name`, under
 * `parent` by Organizes. */
UA_StatusCode shim_server_add_view(UA_Server *server, shim_node node, shim_node parent,
                                   const char *name) {
    UA_ViewAttributes attributes = UA_ViewAttributes_default;
    attributes.displayName = UA_LOCALIZEDTEXT("", (char *)(uintptr_t)name);
    return UA_Server_addViewNode(server, shim_node_id(node), shim_node_id(parent),
                                 UA_NODEID_NUMERIC(0, UA_NS0ID_ORGANIZES),
                                 name_of(node, name), attributes, NULL, NULL);
}

/* Gives the status of `verifyCertificate` of a memory certificate group, the check of
 * the remote certificate of each channel, on `certificate`, or the status of the
 * constructor of the group when it fails. The trust list holds `trusted`, or nothing
 * when it is NULL, and the group reads the time of `el`. */
UA_StatusCode shim_verify_certificate(const UA_ByteString *certificate,
                                      const UA_ByteString *trusted, UA_EventLoop *el) {
    UA_CertificateGroup group;
    UA_StatusCode status = shim_trust(&group, trusted, NULL, el);
    if(status != UA_STATUSCODE_GOOD)
        return status;
    status = group.verifyCertificate(&group, certificate);
    group.clear(&group);
    return status;
}

/* Reads `certificate` as a client reads the certificate that a server sends: the check
 * of a memory group with an empty trust list and the loop `el`, then for each policy on
 * `security`, a channel context with `certificate` as the remote certificate, and the
 * compare of `certificate` with a context on the certificate of `security`. Also reads
 * the application URI of `certificate`. Sets `*verified` to the status of the check
 * and `*named` to the status of the read of the URI, and gives `Good`, or the status
 * of a step that no `certificate` changes. */
UA_StatusCode shim_read_certificate(const UA_ByteString *certificate,
                                    const shim_security *security, UA_EventLoop *el,
                                    UA_StatusCode *verified, UA_StatusCode *named) {
    UA_CertificateGroup group;
    UA_StatusCode status = shim_trust(&group, NULL, NULL, el);
    if(status != UA_STATUSCODE_GOOD)
        return status;
    *verified = group.verifyCertificate(&group, certificate);
    group.clear(&group);
    UA_String uri = UA_STRING_NULL;
    *named = UA_CertificateUtils_getApplicationUri(certificate, &uri);
    UA_String_clear(&uri);
    UA_SecurityPolicy *policies = NULL;
    size_t size = 0;
    status = shim_add_policies(&policies, &size, security, NULL);
    for(size_t i = 0; status == UA_STATUSCODE_GOOD && i < size; i++) {
        UA_SecurityPolicy *policy = &policies[i];
        void *context;
        if(policy->newChannelContext(policy, certificate, &context) ==
           UA_STATUSCODE_GOOD)
            policy->deleteChannelContext(policy, context);
        status = policy->newChannelContext(policy, &security->certificate, &context);
        if(status == UA_STATUSCODE_GOOD) {
            policy->compareCertificate(policy, context, certificate);
            policy->deleteChannelContext(policy, context);
        }
    }
    for(size_t i = 0; i < size; i++)
        policies[i].clear(&policies[i]);
    UA_free(policies);
    return status;
}

/* Gives the status of the constructor of a memory certificate group with an empty trust
 * list and the loop `el`, and clears the group that it makes. */
UA_StatusCode shim_memory_group(UA_EventLoop *el) {
    UA_CertificateGroup group;
    UA_StatusCode status = shim_trust(&group, NULL, NULL, el);
    if(status == UA_STATUSCODE_GOOD)
        group.clear(&group);
    return status;
}
