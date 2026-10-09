- **ACCESS BLOCK (2026-10-08)** `access "<name>" { subjects, select, allow, authority }`
  (C8) gives a `spec::access::Policy` at `<name>.@access`. `subjects` and `select` are
  selectors. `allow` is one action or a list of actions, each a string or a bare word,
  so `["read", "write"]` and `[read, write]` read the same; a repeat is one action, and
  an empty list is `config.empty-allow` (below). A word that is not an action is
  `config.bad-action`. `authority` is optional, an integer from 0 to 255
  (`config.bad-authority`). With no `authority`, a write is capped at `Authority(0)`,
  the least, as default deny gives the least. Lost: an `authority` that `write` makes
  required, a rule that C8 does not have. The action words are a table in `config` until
  a second reader needs them, such as the `plan` output of access; then they move to
  `spec` as `Action::as_str`. Decided by `laptop.architect-2` (2026-10-08T02:41:38Z,
  https://github.com/synnaxlabs/foundation/issues/1017#issuecomment-6051076121).
  The gate: "The gate gives authority 0 no special meaning: such a writer outranks no
  writer and follows GATE RULES, so it takes control when it opens on an index that no
  writer holds." `control` and `home` must not read `Authority(0)` as "may not write" or
  "may not take control". A change to that is a change to GATE RULES, and it goes to
  `laptop.architect`. The quoted sentence supersedes the sentence on the gate in
  https://github.com/synnaxlabs/foundation/issues/1017#issuecomment-6051076121.
  `laptop.architect` decided it and approved the default cap (2026-10-08T05:21:56Z,
  https://github.com/synnaxlabs/foundation/issues/1017#issuecomment-6052936198).
  An `authority` with no `write` in an `allow` that reads is
  `config.authority-without-write`, also `authority = 0`: only a write uses an
  authority, so the value is a mistake. `Policy::new` still sets the authority of a
  policy with no `write` to zero. Lost: no diagnostic, which hides the mistake. Decided
  by `laptop.architect-2` at 2026-10-08T04:00:34Z
  (https://github.com/synnaxlabs/foundation/pull/1781#issuecomment-6051909712).
  It reads two attributes together, so it runs only when each attribute of the block is
  known and reads, as `config::check` states for a whole definition. Decided by
  `laptop.architect-2` at 2026-10-08T04:24:33Z
  (https://github.com/synnaxlabs/foundation/pull/1781#issuecomment-6052187547).
  Supersedes the silent `authority` of
  https://github.com/synnaxlabs/foundation/issues/1017#issuecomment-6051076121.
  `spec::access::Policy::new` refuses an empty `allow` with
  `spec::access::Error::Empty`, and the decoder refuses stored actions that allow
  nothing with
  `spec::definition::Error::Access { at, error }`, as it maps `placement::Error`.
  `config` maps `Error::Empty` to `config.empty-allow` at the span of the `allow` value,
  with the same message and fix, before it checks `authority`, and only when each other
  attribute of the block reads, as `Policy::new` needs each of them. The rule is in
  `spec` once, and `config::plan::check` holds no copy. Supersedes the
  `config.empty-allow` of an empty list whatever the other attributes give, of
  https://github.com/synnaxlabs/foundation/issues/1017#issuecomment-6051076121. Decided by `laptop.architect-2`,
  2026-10-09T00:48:39Z
  (https://github.com/synnaxlabs/foundation/issues/2013#issuecomment-6071969872).
