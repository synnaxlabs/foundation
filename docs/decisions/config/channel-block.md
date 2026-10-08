- **CHANNEL BLOCK (2026-10-08)** `channel "<name>" { kind, ... }` defines one channel
  (S5) at its own name. `kind` is `"index"` or `"data"`, and `"data"` is the default.
  An index takes `error` and `control`. A data channel takes `index` and `data_type`,
  which it needs, and `quality` and `unit`. Each value is a string or a reference.
  `config::check` gives `config::Definition::Channel`, a `spec::channel::Kind<Name>`
  whose edges are names until `plan` gives each channel its key. `Definition::Spec`
  holds each other definition. Each edge must name a channel that a `channel` block of
  the Documents defines, or `check` gives `config.unknown-channel`, at the span of the
  edge, in source order. An edge to a channel that only the stored spec has gives it
  too, until open folders (X28) land: the files list each channel (A2), so the plan
  removes a stored channel that no file has. Decided by `laptop.architect`
  (2026-10-08T13:38:09Z,
  https://github.com/synnaxlabs/foundation/pull/1886#issuecomment-6061116209).
  Supersedes the clause "until #1082" of
  https://github.com/synnaxlabs/foundation/issues/1152#issuecomment-6036793927. Lost:
  `spec::definition::Definition<C = Channel>`, because `plan` would then wrap each of
  the eight variants again to change one. Decided by `laptop.architect-2` (#1152,
  2026-10-07T11:17:44Z,
  https://github.com/synnaxlabs/foundation/issues/1152#issuecomment-6036793927, and
  2026-10-08T00:51:39Z,
  https://github.com/synnaxlabs/foundation/issues/1152#issuecomment-6049880294).
  After a bad `kind`, `check` gives `document.unknown-attribute` for each attribute that
  no kind knows, and leaves each other attribute, the edges too: each belongs to one
  kind, so its problem depends on the kind. Decided by `laptop.architect-2`
  (2026-10-08T05:54:24Z,
  https://github.com/synnaxlabs/foundation/pull/1806#issuecomment-6053360334).
  Supersedes clause 1 of the #1758 ruling, "the edges, as now" (2026-10-08T02:41:50Z,
  https://github.com/synnaxlabs/foundation/issues/1758).
