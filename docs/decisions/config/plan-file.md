- **PLAN FILE (#337, 2026-10-08)** `config::plan::Plan::encode` gives the canonical
  bytes of a plan, and `config::plan::Plan::decode` reads only those bytes and never
  panics. The first byte is the format version, 1. Then the base pointer, the changes in
  name order, and the homes in name order. A `Spec` definition is its `spec` encoding; a
  channel kind holds its edges as names, so the plan still holds no channel key (A4).
  The bytes hold no span. Another version is `plan::Error::Version`, which says to plan
  again; other bytes are `plan::Error::Malformed` at the offset of the field that holds
  the first wrong byte, or of the field that the bytes cut. `decode` checks only the
  form. `definitions` refuses the cases below, and `spec::region::check` and
  `config::plan::check` (below) refuse each problem of the definitions after the plan.
  None checks `homes`, and none refuses
  a change only because its new bytes equal the stored bytes: with a true `old`, at a
  planned name and kind, such a change states nothing false and changes nothing. #337 PR
  2b, which applies the homes, does no check of its own: `Mesh::apply` refuses a name
  that is not an index (`NotIndex`) and, for an index with no home, a home that is no
  member (`UnknownNode`), and gives no home to an index that has one
  (`laptop.architect`, 2026-10-08T18:35:16Z:
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
  `config::plan::check(definitions, members, kinds)` checks the definitions after the
  plan against each rule of `plan` that `spec::region::check` does not hold, with no
  span. It holds now: the kind table accepts the kind and the config of each
  connector, then `config.unplaced`, `config.connector-home`, `config.split-placement`,
  `config.writer-nodes`, and `config.unknown-node`. #2013 PR 2 adds
  `config.duplicate-name`, `config.empty-allow`, `config.subject-is-connector`, and
  `config.private-key`. Apply calls it after `definitions` and before `Mesh::apply`,
  with the members and the kind table of the node that applies, since both can change
  after `plan`. Decided by `laptop.architect-2`, 2026-10-08T21:36:36Z
  (https://github.com/synnaxlabs/foundation/issues/2013). The rules are in `config`
  once, on one model of the definitions that `plan` builds with spans, and both take
  the connectors of an index in name order, so each gives the same problems; approved
  by `laptop.architect-2`, 2026-10-09T00:48:39Z
  (https://github.com/synnaxlabs/foundation/issues/2013#issuecomment-6071969872).
