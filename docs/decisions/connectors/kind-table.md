- **KIND TABLE** `kind::Kind` is typed: an associated `Config` and `impl Future`
  methods. `kind::Table` erases it inside `connector` with a private trait that takes
  the `Document` and parses again, so callers see one concrete type with no `Any` and
  no downcast. An unknown kind is the diagnostic `connector.unknown-kind`, since the
  name comes from a file. A run or a discovery fails with one of three classes:
  `Config`, `Device`, and `Retry` (restart with backoff). Decided by the `connector`
  builder in the plan on #338, after `/eb-review`; approved by the coordinator
  (#338). After `Config`, the connector stops until a spec change changes it
  (`laptop.architect-2`, 2026-10-09T05:10:43Z:
  https://github.com/synnaxlabs/foundation/pull/2056#issuecomment-6074718616).
  Supersedes "The supervisor stops; a spec change starts it again" of the plan on #338
  (https://github.com/synnaxlabs/foundation/issues/338#issuecomment-5994872059).
  `Table::check` takes where the file names the kind and puts `connector.unknown-kind`
  there; `discover` and `run` take their kind from the spec, which has no spans
  (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152,
  2026-10-08 03:02 UTC). `Table::check` also puts there each diagnostic of the kind
  with no span, since a `Document` has none to place a missing attribute
  (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/pull/1782#issuecomment-6051900967,
  2026-10-08 03:59 UTC).
  `Channels::counts` names the count channels of the kind,
  `<connector>.status.<count>`. `connector::status` names the status index (`time`)
  and the supervisor's channels (`state`, `class`, `restarts`, `backoff`, `error`)
  once, for the supervisor and for `config`. `Table::check` panics when a kind names a
  count that `connector::status` names in any case, a count of more than one segment,
  or one count twice in any case, since the kind's code is wrong (`laptop.architect-2`,
  2026-10-08T03:05:58Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6051331538,
  change 4 and answer 2; `laptop.architect-2`, 2026-10-09T13:37:08Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6082035824;
  `laptop.architect-2`, 2026-10-09T13:58:16Z:
  https://github.com/synnaxlabs/foundation/pull/2150#issuecomment-6082402451; for
  `backoff` and `error`, `laptop.architect-2`, 2026-10-08T06:29:21Z:
  https://github.com/synnaxlabs/foundation/issues/1735#issuecomment-6053869186).
  `node` builds the table once, in `kinds`, and gives `ops::Node` an
  `Arc<kind::Table>`, the type that `supervisor::Config` takes, so that each kind is
  one value for the life of the process (`laptop.architect-2`, 2026-10-10T15:29:33Z:
  https://github.com/synnaxlabs/foundation/pull/2252#issuecomment-6099106464).
  The supervisors get the same `Arc`, from the one wiring site (`laptop.director`,
  2026-10-09T05:05:48Z:
  https://github.com/synnaxlabs/foundation/issues/1156#issuecomment-6074666909).
