# Where things are defined


Storage classes used in the table:

- **Files**: definition files, read through the Document model.
- **Spec**: the stored spec. One prolly tree per region in `blob`; the pointer (version
  and root hash) is in that region's Raft state.
- **Region state**: runtime state agreed by one region's voters through Raft, outside
  the spec.
- **Index log**: records in an index's log in `buffer` at the home, copied by `replica`.
- **Channel**: values published as samples.
- **Memory**: in memory on one node; not durable.
- **Node-local**: on one node's disk or local config; not agreed.
- **Kind-owned**: an opaque Document inside the spec that only the kind decodes.
- **Binary**: compiled into the binary.
