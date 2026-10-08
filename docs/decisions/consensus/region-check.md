- **REGION CHECK (#1841)** `spec::region::check(prefix, definitions)` gives each
  problem of a region's definitions, by tree key, in tree key order: each
  `spec::channel::check` problem, each name that the region does not govern (X2), and
  each definition that is not at the tree key of its kind. `Mesh::apply`, #1741, the
  node state of BQ11b, and `plan` (#1082) call it. A channel's edges point only at
  channels of its own region, so a region checks its spec alone, also while cut off
  (K5), and a change in one region breaks no edge of another. Two channels with one key
  are `channel::Problem::Duplicate`, not a panic, as a committed spec comes from other
  nodes. Lost: an input of the keys of other regions, so that an edge may cross
  regions; a `spec::Region` that cannot hold a problem, as #1741 and BQ11b keep a
  committed spec with problems; and a module `spec::problem`. The reach of a policy
  (X26) is not in it yet: #1846 adds it, and `config` (#679) gives its diagnostic from
  that problem (`laptop.architect-2`, 2026-10-08T09:11:05Z,
  https://github.com/synnaxlabs/foundation/issues/1841#issuecomment-6056600033).
  `spec::region::tree(chunks, definitions)` builds the tree of a region's definitions,
  and cannot fail. `mesh` calls it at open and at apply. Lost: the function in
  `spec::tree`, which then points at the model above it; and the encode in the caller.
  `spec::region::definitions(chunks, root)` reads a region's tree back into its
  definitions, the inverse of `tree`. `mesh` calls it for each new spec
  (`laptop.architect-2`, 2026-10-08T11:11:54Z,
  https://github.com/synnaxlabs/foundation/issues/1741#issuecomment-6058593807).
  It refuses a tree that is not the tree of its definitions (`laptop.architect-2`,
  2026-10-08T14:05:00Z,
  https://github.com/synnaxlabs/foundation/pull/1891#issuecomment-6061622909).
  `plan` (#1082) maps a key to its region with the function of `spec::region`, and
  keeps no copy (`laptop.architect-2`, 2026-10-08T09:09:16Z,
  https://github.com/synnaxlabs/foundation/pull/1844#issuecomment-6056571263). The
  check of the key form accepts a reserved label only at a founding key (FIRST ADMIN;
  `laptop.architect`, 2026-10-08T12:51:04Z,
  https://github.com/synnaxlabs/foundation/pull/1880#issuecomment-6060256808). Each
  other definition at a reserved label is `Misplaced`, so a region there makes no
  child region. Supersedes "only for a subject and an access policy, the kinds of the
  founding definitions" (`laptop.architect-2`, 2026-10-08T09:51:31Z,
  https://github.com/synnaxlabs/foundation/pull/1844#issuecomment-6057256316), as
  `ops.@x.@subject` then passed (#1877;
  `laptop.architect`, 2026-10-08T12:36:57Z,
  https://github.com/synnaxlabs/foundation/pull/1862#issuecomment-6060015621). A file
  still cannot hold a reserved label (`Kind::key`).
  Lost: a check that skips each reserved key, as a channel at `@admin.@subject` is
  then no problem and the check needs `spec::key::reserved`. Decided by
  `laptop.architect-2`, 2026-10-08T09:15:07Z
  (https://github.com/synnaxlabs/foundation/pull/1844#issuecomment-6056664804), and
  the kinds 2026-10-08T09:51:31Z
  (https://github.com/synnaxlabs/foundation/pull/1844#issuecomment-6057256316),
  superseded by
  https://github.com/synnaxlabs/foundation/pull/1862#issuecomment-6060015621.
  Supersedes: the panic for two channels with one key (architect, #756,
  https://github.com/synnaxlabs/foundation/issues/756#issuecomment-6031836890), and
  `Problem::Shared` with the fix "Give each channel its own key", which replaced it
  (architect, 2026-10-07T06:47:45Z,
  https://github.com/synnaxlabs/foundation/issues/756#issuecomment-6032581487); the
  supersede of `Problem::Shared`: `laptop.architect-2`, 2026-10-08T11:11:18Z
  (https://github.com/synnaxlabs/foundation/pull/1844#issuecomment-6058584682).
  Decided by `laptop.architect-2`: the check, 2026-10-08T08:41:52Z
  (https://github.com/synnaxlabs/foundation/issues/1841#issuecomment-6056118794); the
  tree, 2026-10-08T08:47:57Z
  (https://github.com/synnaxlabs/foundation/issues/1841#issuecomment-6056217372). On
  the checks in `spec` of `laptop.architect`, change 2, 2026-10-08T08:22:08Z
  (https://github.com/synnaxlabs/foundation/issues/1083#issuecomment-6055806836), and
  the tree of the founding definitions, 2026-10-08T08:41:43Z
  (https://github.com/synnaxlabs/foundation/pull/1840#issuecomment-6056116151). The
  supersede and the edge rule, agreed by `laptop.architect`, 2026-10-08T08:43:31Z
  (https://github.com/synnaxlabs/foundation/issues/1841#issuecomment-6056144931).
