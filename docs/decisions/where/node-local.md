# Connectors, time, status, and node-local state

| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Kind | Binary: one literal table built in `node` | The build | `config` (check), `ops` (discover), supervisor (run) | `connector` (contract), `connector-<kind>` |
| Commandable parameter | The kind declares it; the connector config marks it commandable; value = a channel plus an ack under the connector's name; files give the starting value (index layout: X24) | Subjects with access and authority | The kind, through `ctx` | `connector` (library), the kind |
| Run state | The `running` commandable parameter | As above | As above | `connector` |
| Shared endpoint | Memory: an endpoint registry on the kind value (process lifetime) | The kind | Connectors of that kind on the node | `connector` (endpoint component) |
| Connector status channels | Channels under the connector's name | The kind through `ctx.status()` | People, agents, tools | `connector` |
| Node status channels | Channels under the node's name (definitions: X27) | `node`'s collector through `hub`, from each crate's pulled values | People, agents, tools, rebalancers | `node` |
| Quarantine | Per out connector: a hold on the original data plus an error record (samples on a channel under the connector's name); size on a status channel | The kind, through a library component | `ops` list, retry, drop | `connector` |
| Secret value | Never in files, plans, or output. Built-in store: region state ciphertexts. External stores through adapters. References by name in kind config | `secret set` (person or CI) | `ctx.secret()` on the connector's node | `secret` (seal and open; `ops` seals, `node` opens), resolver (X40) |
| Time sources | Binary: a source table built in `node`; adapters probe for hardware | Adapters feed measurements | The estimator | `clock` (adapters), estimator crate (X11) |
| Mesh clock state | Memory per node; any shard reads its time and status (`Reader::now`, `Reader::status`); published as `<node>.clock.offset`, `.clock.error`, and the status | `clock`; `node` publishes | `hub.now()`, `home` (fence, stamp limits) | `clock` |
| Operation table | Binary | The build | CLI, MCP, embedded docs | `ops` |
| Node key material | `node.key` on node-local disk | `node` (makes it at its first start or with `node::create_key`, writes it back at each start) | `transport`, `node` | `node` |
| Per-node settings (disk budget, pool budget, data directory) | Budgets: a policy in the spec; data directory: a start argument (NODE SETTINGS) | `apply`; whoever starts the node | `buffer`, `block` | `node` |
| SDK guide, JSON schemas for editors | Generated from kinds and the operation table | `ops`, `init` | Agents, editors | `ops` |
