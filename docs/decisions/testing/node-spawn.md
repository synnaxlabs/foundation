- **NODE SPAWN (2026-10-07)** `Node::spawn(task)` calls `task` with the node's one hub,
  on shard 0, once each shard has opened its buffer, then runs its future. It has the
  shape and the rules of `env::tasks::Tasks::spawn`: no handle, `Output = ()`, and a
  panic ends shard 0 and fails the node (`Error::Panicked`), unless the transport
  stopped first, which gives `Error::Transport`. Shard 0 calls the tasks
  with the hub in the order of their calls, so their closure bodies run in that order;
  their futures run in no set order. A task that is given before the hub exists waits
  for it. A node that stops or fails before shard 0 calls a task drops it uncalled, and
  a stop drops each running future. A future that completes drops at once. The task runs
  on shard 0's thread, so it may hold values that are not `Send`, such as sessions.
  `node` depends on `hub`, and `Node::interner` goes away: shard 0 builds the hub with
  the interner when it comes back from the last shard. Decided by `laptop.architect-2`
  (2026-10-07T18:04:23Z):
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6043838411.
  `hub::Config` stays as it is, one interner by value for one shard, and sessions on the
  home of each shard wait for #1566. Decided by `laptop.architect`
  (2026-10-07T18:02:03Z):
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6043797070, with the
  director's OK for the deferral (2026-10-07T18:15:18Z):
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6044020168.
  The call order and no result decided by `laptop.architect-2` (2026-10-07T19:49:12Z):
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6045597072.
  Supersedes the start order of
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6043838411. When a
  caller outside the tests of `node` builds a result channel, file an `interface` issue
  for a result from `spawn`. `Error::Transport` over a panic after the transport stopped
  decided by `laptop.architect-2` (2026-10-08T03:21:42Z):
  https://github.com/synnaxlabs/foundation/pull/1769#issuecomment-6051497737.
  Supersedes the panic error of
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6043838411.
  Amended (2026-10-10, #2266, by `laptop.architect-2`): shard 0 calls a task only once
  the node takes sessions, as well as once each shard has opened its buffer. Approval
  owed. Supersedes the start order of the first sentence of this record.
