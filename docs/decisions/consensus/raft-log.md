- **RAFT LOG (#91)** A leader takes `propose(data)` and returns the entry's `Position`,
  or `Error::NotLeader { leader }` with the leader it knows. A new leader writes an
  empty entry of its term first, so it can commit what came before. It replicates with
  `Body::Append { prev, entries, commit }`, answered by `Body::AppendReply { last }`
  (the last index the follower holds of what was sent) or `Body::AppendReject { hint }`
  (its hint for the next `prev`). `step` checks a message against the log before it
  changes state: entries that do not follow `prev` are `Error::EntryOutOfOrder`, and a
  heartbeat's `commit`, an append reply's `last`, or an append reject's `hint` past the
  log is `Error::IndexPastLog`, unless `step` drops the reply (RAFT SURFACE). An
  append's `prev` and `commit` and a vote's `last` can be past the log of a node that is
  behind. An `Append` with an entry whose term is above the message's term is
  `Error::TermBehindLog`: no leader sends one, so the sender is faulty. The conformance
  oracle changed to match; the person decided on 2026-10-05 ("a is fine", #232). A
  heartbeat or an append of this node's term from a node other than the leader it knows
  is `Error::SecondLeader`: one term has one leader, and a node keeps the leader of its
  term until the term ends, through a step-down and a campaign. A node that knows no
  leader of its term takes the first that proves a quorum of its votes, else
  `Error::Unproven`; `Hard.leader` keeps it through a restart (#750). The person
  approved it on 2026-10-05 ("Yeah that's fine", #391). A bad message changes nothing.
  A voter that does not lead cannot make a node follow it: a leader claim needs a
  quorum of grants (RAFT SURFACE, #750), except a voter that led a term at or above
  the node's committed one, which can forge a link until #882 (RAFT SURFACE). After a
  restart, the committed term is the term at the applied index, because `Hard` holds no
  commit index. That term can be lower than the term at the commit index before the
  restart, so more past leaders can forge a link. Lost: the commit index in `Hard`. It
  costs one more durable write each time the commit index moves, for a gap that #882
  closes. Also lost: a bound of the highest term in the stable log. It refuses a real
  leader whose link has a lower term than an entry of the node that is not committed.
  Decided by `laptop.architect` (#1682, 2026-10-08T01:03:46Z):
  https://github.com/synnaxlabs/foundation/pull/1682#issuecomment-6050014758.
  A false `AppendReply` still counts as held (#882). Lost: a lease that drops a
  heartbeat or an `Append` of a higher term from a node that is not the leader. A
  reply of a higher term ends any node's lease, and a leader must step down on one;
  the lease also changed three etcd oracle tests. The
  coordinator decided on 2026-10-06 under the person's delegation (#391). The person
  may change it.
  `Body::Heartbeat { commit }` carries the commit index, capped at what that follower
  is known to hold. A leader commits an index only when a quorum holds it and its
  entry is of the leader's own term. A follower commits no further than the last
  entry the leader sent it. `Ready.committed` gives each entry once, after it is
  written. Batch size (64 entries) and the number of appends in flight per follower
  (8) are constants, not `Config` fields: nothing measured asks for a knob. `Message`
  and `Body` are `Clone`, not `Copy`, because an append carries entries.
