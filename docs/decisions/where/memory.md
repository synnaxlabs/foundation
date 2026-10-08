# Sessions and values in memory

| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Frame | Memory: one pool block (FRAME LAYOUT): a header (key set key, form, path), a range `{ group, count, seq }` for each present index group (X8), and a descriptor `{ entry, end }` for each present series, each list sorted. Wire form per connection. Never stored as a frame on disk | Writers, through `hub.block` or the frame builder; `home` (INDEX FRAMES) | `delivery` views, `hub`, `codec` | `types` (layout), `block` (memory) |
| Series | Memory: a slice of the frame's block. Encoded: tagged 1024-value vectors | Writers; `codec` | Readers | `types`, `codec` |
| Block | Memory: per-shard pools that `node` injects | Writers fill a `Unique`, then freeze it | Every holder, by refcount | `block` |
| View | Memory: none; borrows a frame and a mask | `delivery` | The reader session | `types` (value), `delivery` |
| Reader session | Memory: `hub` session (selector expansion, max-age check); per-index state in `delivery` at the home or copy | The reader (SDK, CLI, connector) | `hub`, `delivery` | `hub`, `delivery` |
| Writer session | Memory: `hub` session (key set, routing, confirmation); gate in `control`; seq and dedup in `home` | The writer | `hub`, `control`, `home` | `hub`, `control`, `home` |
| Subscription | The selector of a reader session, kept live in `hub` against `mesh` watches | The reader | `hub` | `hub` |
| Effective settings | Memory: a per-node cache of `spec::resolve` results | `mesh` | `home`, `transport`, `clock`, supervisor | `mesh` |
| Document | Memory: made by a front end from files, or by SDK code | Front ends | `config`, kinds | `document` (X21) |
| Diagnostic | Memory: made from a producer's error | Front ends, kinds, `document` | `config`, `ops` (text, `--json`, MCP) | `document` (DIAGNOSTICS) |
| Selector | A value inside policies, readers, connectors, and access, kept as written. Equality compares the texts in order, so equal selectors encode to equal bytes (#836) | Files, sessions | Every matcher | `types` (one matcher) |
| Plan | A JSON artifact with stable change kinds | `ops plan` | `ops apply` (commits exactly it) | `config`, `ops` |
