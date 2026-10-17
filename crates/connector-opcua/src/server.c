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
#include "server/ua_server_internal.h"
#pragma GCC diagnostic pop

#include <string.h>

#include "shim.h"

/* Gives a server on `el` with the minimal config, the one server URL
 * `opc.tcp://{host}:{port}`, room for `sessions` sessions and secure channels, and at
 * most `chunks` chunks in a message, or NULL on a failure. */
UA_Server *shim_server_new(UA_EventLoop *el, UA_UInt16 port, const char *host,
                           UA_UInt16 sessions, UA_UInt32 chunks) {
    UA_ServerConfig config;
    memset(&config, 0, sizeof(config));
    /* The loop of `shim_loop_new` owns its logger, which is not const. */
    config.logging = (UA_Logger *)(uintptr_t)el->logger;
    config.eventLoop = el;
    config.externalEventLoop = true;
    if(UA_ServerConfig_setMinimal(&config, port, NULL) != UA_STATUSCODE_GOOD) {
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

/* The counts of a `shim_counter`. */
typedef struct {
    /* The Browse requests. */
    size_t browses;
    /* The most continuation points that one session held at a message. */
    size_t points;
} shim_counts;

/* A logger that counts the Browse requests of its server and the continuation points
 * of its sessions, and passes each message to the logger before it. */
typedef struct {
    UA_Logger logger;
    const UA_Logger *next;
    UA_Server *server;
    shim_counts counts;
} shim_counter;

static void count_message(void *context, UA_LogLevel level, UA_LogCategory category,
                          const char *msg, va_list args) {
    shim_counter *counter = (shim_counter *)context;
    /* The end of the format of the debug message of open62541 for each Browse
     * request, from UA_LOG_SESSION_INTERNAL. */
    static const char browse[] = "| Processing BrowseRequest%.0s";
    size_t length = strlen(msg);
    if(length >= sizeof(browse) - 1 &&
       strcmp(msg + length - (sizeof(browse) - 1), browse) == 0)
        counter->counts.browses++;
    session_list_entry *entry;
    LIST_FOREACH(entry, &counter->server->sessions, pointers) {
        size_t held = entry->session.continuationPointsSize;
        if(held > counter->counts.points)
            counter->counts.points = held;
    }
    counter->next->log(counter->next->context, level, category, msg, args);
}

static void count_clear(UA_Logger *logger) { UA_free(logger->context); }

/* Counts each Browse request that `server` processes from now, and the most
 * continuation points that one of its sessions holds, and gives the counts, which live
 * until the delete of `server`, or NULL when out of memory. */
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

/* Sets the receive buffer size that `client` gives in its next HEL. */
void shim_client_receive(UA_Client *client, UA_UInt32 size) {
    UA_Client_getConfig(client)->localConnectionConfig.recvBufferSize = size;
}

/* The node context of a variable of `shim_server_add_source`, as the first field of
 * the context that Rust holds. */
typedef struct shim_source shim_source;
struct shim_source {
    /* Gives the value, its kind, and the status of a read of the variable. */
    void (*read)(shim_source *source, shim_scalar *value, UA_UInt16 *kind,
                 UA_StatusCode *status);
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
    (void)includeSourceTimeStamp;
    /* The client of the crate reads no range. */
    (void)range;
    value->hasStatus = true;
    shim_source *source = nodeContext;
    shim_scalar read;
    UA_UInt16 kind;
    source->read(source, &read, &kind, &value->status);
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

/* Gives the status of `verifyCertificate` of a memory certificate group with an empty
 * trust list, the check of the remote certificate of each channel, on `certificate`. */
UA_StatusCode shim_verify_certificate(const UA_ByteString *certificate) {
    UA_CertificateGroup group;
    memset(&group, 0, sizeof(group));
    UA_TrustListDataType trust;
    UA_TrustListDataType_init(&trust);
    UA_NodeId id = UA_NODEID_NUMERIC(
        0, UA_NS0ID_SERVERCONFIGURATION_CERTIFICATEGROUPS_DEFAULTAPPLICATIONGROUP);
    UA_StatusCode status =
        UA_CertificateGroup_Memorystore(&group, &id, &trust, NULL, NULL);
    if(status != UA_STATUSCODE_GOOD)
        return status;
    status = group.verifyCertificate(&group, certificate);
    group.clear(&group);
    return status;
}
