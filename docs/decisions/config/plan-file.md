- **PLAN FILE (#337, 2026-10-08)** `config::plan::Plan::encode` gives the canonical
  bytes of a plan, and `config::plan::Plan::decode` reads only those bytes and never
  panics. The first byte is the format version, 1. Then the base pointer, the changes in
  name order, and the homes in name order. A `Spec` definition is its `spec` encoding; a
  channel kind holds its edges as names, so the plan still holds no channel key (A4).
  The bytes hold no span. Another version is `plan::Error::Version`, which says to plan
  again; other bytes are `plan::Error::Malformed` at the offset of the field that holds
  the first wrong byte, or of the field that the bytes cut. `decode` checks only the
  form. `definitions` refuses the cases below, and `spec::region::check` and
  `config::plan::check` (below) refuse problems of the definitions after the plan. None
  checks `homes`, and none refuses a change only because its new bytes equal the stored
  bytes: with a true `old`, at a planned name and kind, such a change states nothing
  false and changes nothing. #337 PR 2b, which applies the homes, does no check of its
  own: `Mesh::apply` refuses a name that is not an index (`NotIndex`) and, for an index
  with no home, a home that is no member (`UnknownNode`), and gives no home to an index
  that has one (`laptop.architect`, 2026-10-08T18:35:16Z:
  https://github.com/synnaxlabs/foundation/issues/1931#issuecomment-6066553633). No
  check of its own decided by `laptop.architect-2`, 2026-10-08T21:30:44Z
  (https://github.com/synnaxlabs/foundation/pull/2007#issuecomment-6069434343), which
  changes "#337 PR 2b, which applies the homes, checks each one" of
  https://github.com/synnaxlabs/foundation/pull/1970#issuecomment-6067669742.
  `config::plan::Plan::definitions(applied, key)` gives those definitions with the key
  rule of PLAN SURFACE, and an edge to no channel gets a key from `key`, which the check
  refuses as dangling. Each call of `key` must give a key that no channel holds and that
  no earlier call gave. `definitions` is fallible: it refuses with
  `plan::Error::Mismatch { name }` at the first change in one of these cases, which
  `plan` never makes from `applied`, so only a hand-made file holds. The change's `old`
  is not the digest of the stored definition at its name, or the stored or new
  definition is of a kind that no block defines or is not at the tree key of an
  unreserved label of its kind. The rule stays in `config`, in the one place that holds
  `applied`; apply still checks `base` first. The codec copies the channel kind layout
  of `spec::definition`; #1975 gives `spec` the bytes of `channel::Kind<E>`, at the next
  change to the channel kind format of `spec` or at a second user of the bytes of
  `Kind<Name>`. Each item of a plan has one path, under `config::plan`. `Plan::changes`
  is a `BTreeMap<Name, Change>`, and `Change` holds no name, so two changes at one name
  cannot exist; `decode` still refuses a repeated name as `Malformed`. Plan:
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6066674221. Decided
  by `laptop.architect-2`: the three methods and the version byte (2026-10-08T16:00:18Z,
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6063892745); one
  path, the variants `Version` and `Malformed`, the new key for a dangling edge, and the
  `key` contract (2026-10-08T18:45:40Z,
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6066727322);
  `Mismatch`, a fallible `definitions`, and the #1975 deferral (2026-10-08T19:11:26Z,
  https://github.com/synnaxlabs/foundation/pull/1970#issuecomment-6067164684); a change
  at a reserved name or of a blockless kind is a `Mismatch` (2026-10-08T19:20:33Z,
  https://github.com/synnaxlabs/foundation/pull/1970#issuecomment-6067332179); so is one
  at a name that is not the tree key of its kind, with one predicate for `plan` and
  `definitions` (2026-10-08T19:27:39Z,
  https://github.com/synnaxlabs/foundation/pull/1970#issuecomment-6067449819);
  `Mismatch` refuses only a false `old`, a reserved label, a blockless kind, or a name
  that is not the tree key of its kind, and the check of the homes
  (2026-10-08T19:40:40Z,
  https://github.com/synnaxlabs/foundation/pull/1970#issuecomment-6067669742); `changes`
  keyed by name (2026-10-08T19:54:45Z,
  https://github.com/synnaxlabs/foundation/pull/1970#issuecomment-6067900033).
  Supersedes the `Mismatch` Display text and case list of
  https://github.com/synnaxlabs/foundation/pull/1970#issuecomment-6067164684. Supersedes
  the rule "a change that `plan` cannot make from `applied`" of
  https://github.com/synnaxlabs/foundation/pull/1970#issuecomment-6067332179. Supersedes
  the `changes: Vec<Change>` field and `Change::name` of
  https://github.com/synnaxlabs/foundation/issues/1082#issuecomment-6040866688.
  The `ops` apply decodes the plan, compares its base with the spec pointer, then calls
  `definitions`, as `definitions` reads the definitions at the base. So a stale plan
  with a reserved change gives `ops.stale-plan` (OPS OUTPUT). Decided by
  `laptop.architect-2` (2026-10-08T22:57:59Z,
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6070706432).
  Supersedes the order of
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6060043966.
  A plan with no change and no home proposes nothing: after the `ops.behind` check and
  the base compare, the apply gives the pointer in use, so it makes no other plan
  stale. Such a plan at an old base gives `ops.stale-plan`, as its files can differ
  from the newest spec. A hand-made plan that lists no home for a new index applies,
  and the index has no home until a later change gives one (SPEC APPLY). Decided by
  `laptop.architect-2` (2026-10-08T23:51:37Z, items 3 and 4 of
  https://github.com/synnaxlabs/foundation/pull/2035#issuecomment-6071339913).
  `config::plan::check(definitions, members, kinds)` checks the definitions after the
  plan, with no span. It gives the first stage with problems: `config.private-key` for
  each string of a definition; then, together, what the kind table refuses in the kind
  and config of each connector, `config.duplicate-name`, and
  `config.subject-is-connector`; then `config.unplaced`, `config.connector-home`,
  `config.split-placement`, `config.writer-nodes`, and `config.unknown-node`.
  `spec::access::Policy::new` refuses an empty `allow`, so `Plan::decode` refuses it.
  #2013 PR 3 makes apply call `check` after `definitions` and before
  `Mesh::apply`, with the members and the kind table of the node that applies, as both
  can change after `plan`. Decided by `laptop.architect-2`, 2026-10-08T21:36:36Z
  (https://github.com/synnaxlabs/foundation/issues/2013). The rules are in `config`
  once, on one model of the definitions that `plan` builds with spans, and both take the
  connectors of an index in name order, so each gives the same problems. Name order, and
  an empty `allow` refused in `spec`: `laptop.architect-2`, 2026-10-09T00:48:39Z
  (https://github.com/synnaxlabs/foundation/issues/2013#issuecomment-6071969872).
  This changes the first-writer order of
  https://github.com/synnaxlabs/foundation/pull/1886#issuecomment-6061802143. A message
  of a later stage can quote a string of a definition, so no later stage runs on a key.
  The problems of the second stage each read one definition or one name. The plan rules
  read the kind of each connector and each name, so an unknown kind or a duplicate name
  gives false problems there. Lost: one stage with all problems. The stages: decided by
  `laptop.architect-2`, 2026-10-09T01:57:07Z
  (https://github.com/synnaxlabs/foundation/pull/2067#issuecomment-6072680913).
