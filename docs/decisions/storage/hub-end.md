- **HUB END (#585)** The hub's commit task holds the hub's state weakly, and keeps its
  waker in the state while it sleeps and while it waits for a commit. The state wakes
  it on drop, and the task ends at its first poll after that. Lost:
  `Hub::close(self) -> Commit`, which each caller must call, and which a clone or a
  live session defeats. Decided by `laptop.architect` (2026-10-07T18:07:55Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6043897000).
  The commit that the task waits for lives in the state, and the task polls it through
  the state. So the drop of the state drops the commit in the same call. Once the hub
  and each of its sessions drop, the hub holds no part of the home: no `Home`, no
  `Commit`, no `Reading`. A task that the hub spawns holds a part of the home only
  through the state or a session. `node` takes its own commit before it gives the home
  to the hub, drops the hub and each session, awaits the commit, which resolves once the
  buffer's task ended, drops it, and then lets go of the data directory lock. Lost: a
  future of the end of the task, one more step for each caller; and an order in `node`,
  which cannot know what the hub holds. Supersedes: "So the task ends, and drops the
  commit it waits for, at its first poll after the hub and each of its sessions drop"
  (https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6043897000).
  Decided by `laptop.architect` (2026-10-07T21:23:22Z:
  https://github.com/synnaxlabs/foundation/issues/585#issuecomment-6047128783).
  The future of `Hub::serve` holds a hub clone and a session, so it keeps the hub's
  state and the commit task alive. `node` keeps each such future where `keep` drops it,
  after the guard and before it awaits the commit, and never runs one with
  `tasks.spawn`, which gives no way to drop it. Decided by the architect
  (2026-10-07T21:34:19Z,
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6047300641).
  Changed by HUB LINK (#1946): `Hub::serve` is gone. A `Link`, the future of
  `Link::serve`, and a `Reply` each hold the hub's state, so this entry counts each as
  a session of the hub. `node` drops each where `keep` drops the future, before it
  awaits the commit, and never runs a future of `Link::serve` with `tasks.spawn`.
  Decided by `laptop.architect` (2026-10-08T18:16:38Z,
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066239520).
  The state holds a clone of the region's mesh when the hub has one. Decided by
  `laptop.architect` (2026-10-08T18:42:42Z,
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6066677536).
  With PR 4d-b of #340, the state holds the region (the mesh and an `Rc` of the
  shard's transport), and holds no session: the transport keeps the one session to
  each node (ONE SESSION PER PEER). When that session closes with `Code(0)` before the
  home's `Opened`, as a session that loses the tie-break of ONE SESSION PER PEER does,
  the task dials once more and opens on the session that this dial gives. The home never
  served an open on the losing session: the lower node holds its streams until it closes
  it. Amended by `laptop.architect` (2026-10-09T09:00:46Z,
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6077794101). A remote
  reader holds the state, so it counts as a session of the hub. Decided by
  `laptop.architect`: the region (2026-10-08T20:07:32Z,
  https://github.com/synnaxlabs/foundation/issues/340#issuecomment-6068108715), and no
  session in the hub (2026-10-08T21:19:24Z,
  https://github.com/synnaxlabs/foundation/pull/2003#issuecomment-6069259471).
