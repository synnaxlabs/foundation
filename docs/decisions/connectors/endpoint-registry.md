- **ENDPOINT REGISTRY** `endpoint::Registry<K, S, T>` keeps at most one open
  endpoint per key on a node. `acquire(key, settings, open)` shares the open endpoint,
  or calls `open` when none is open. Opens and closes of one key run one at a time;
  other keys do not wait. Unequal settings on an open key give `Error::Config`
  (`connector.endpoint-settings`). The endpoint closes when the last `Lease` drops.
  `node` makes one registry per kind that needs it. A FIFO lock (`endpoint::Shared`)
  composes as `T` later. A `Lease` is not `Clone`, and the close runs after the
  registry's lock is released. Decided by the `connector` builder in the plan on #422,
  after `/eb-review`; approved by the coordinator (#422).
