/* The allocator of the copy, `shim.c`, and `server.c`: the functions of
   `src/alloc.rs`, on the global allocator of the binary. `config.h` of the copy
   includes this file as its `UA_ARCH_HEADER`, before it defines the libc calls as
   the defaults. */
#ifndef CONNECTOR_OPCUA_ALLOC_H
#define CONNECTOR_OPCUA_ALLOC_H

#include <stddef.h>

void *connector_opcua_malloc(size_t size);
void *connector_opcua_calloc(size_t count, size_t size);
void *connector_opcua_realloc(void *ptr, size_t size);
void connector_opcua_free(void *ptr);
/* `free` for a block of which C wrote only the first `written` bytes: it clears only
   those. */
void connector_opcua_free_written(void *ptr, size_t written);

#define UA_malloc connector_opcua_malloc
#define UA_calloc connector_opcua_calloc
#define UA_realloc connector_opcua_realloc
#define UA_free connector_opcua_free

#endif
