- **POLICY NAMES (2026-10-05)** The label of a policy is a name (A3), unique among the
  policies of its kind. Its tree key `<label>.@<kind>` is a name too, so a label holds
  at most 255 bytes less the suffix (240 for `node_settings`). A policy name can equal a
  channel name. A policy belongs to the region that governs its name (X2: the longest
  region prefix that contains it), and it may select only names in that region and its
  descendants (X26). When a `region` block is added or removed, `plan` checks X26 again
  for each policy whose region changes, lists each policy that moves to other voters,
  and refuses one whose reach fails. Lost: the region from the selector (a wider pattern
  would move the policy to other voters silently, and X26 could never fail), and the
  region from the directory (K2 makes the layout a default only; r3 rejected a
  `region =` attribute). The advisor approved it on 2026-10-05, #474.
  The `<kind>` segment of each kind is its HCL keyword: `@access`, `@region`,
  `@node_settings`, `@compression` (compression section), `@placement` (S12),
  `@retention` (#895), `@subject`, and `@time`. No time keyword was on record (C6 shows
  `[[time]]`, and X36 replaced its content), so the architect decided `time`.
  `laptop.architect-2` decided `@subject` at 2026-10-08T03:15:41Z
  (https://github.com/synnaxlabs/foundation/issues/1755#issuecomment-6051435217).
  A connector has no segment: it is at its own name, and its channels are its children
  (#758, 2.2, C8). A channel has no segment either: it is at its own name (#756,
  https://github.com/synnaxlabs/foundation/issues/756#issuecomment-6031378098). The `@`
  check still applies to both names. A region record is at `<prefix>.@region` in the
  parent's tree (#758). This is not an exception to X2: the region that holds the record
  is the longest region prefix that contains `<prefix>`, other than `<prefix>` itself.
  The root region has no record and no key: no parent records it (X3), and its voters
  live only in its Raft config. The one place that maps a key to its region applies
  this, so no caller tests for `@region`. Decided by the architect, #1001
  (https://github.com/synnaxlabs/foundation/issues/1001#issuecomment-6031305302; #758
  for the connector and the region). The kind is `spec::definition::Kind`, and the
  module `spec::key` holds the whole key rule: the segments, the `@` rule, the bound,
  `Kind::key`, and `key::Error`. Lost: a module `spec::kind`, because in `spec` "kind"
  also names a connector's driver. Decided by the architect, #1109
  (https://github.com/synnaxlabs/foundation/pull/1109#issuecomment-6031286198 and
  https://github.com/synnaxlabs/foundation/pull/1109#issuecomment-6031290037).
  `Kind::key` takes the label as text and checks it in this order: the bound
  (`key::Error::Long`, the one length error for every kind, with `Name::MAX_BYTES` for a
  connector), then the name (`key::Error::Name`), then the `@` rule. So the user gets
  the true bound in one round. Lost: a `&Name` label, whose parse gives its own length
  error with the wrong bound. Decided by the architect, #1109
  (https://github.com/synnaxlabs/foundation/pull/1109#issuecomment-6031559597).
