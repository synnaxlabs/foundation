- **CONNECTOR BLOCK (2026-10-08)** `connector "<name>" { kind, node, ... }` (X22)
  gives a `spec::connector::Connector` at its own name, which is unique in any case
  among the keys of every block. `kind` and `node` are names, and each is required. The
  config is the body without `kind` and `node`. `config::check` takes a
  `connector::kind::Table`, and the kind that `kind` names checks the config through
  `Table::check`, with `at` the span of the `kind` value. A kind that the table does
  not have is `connector.unknown-kind` there. Decided by `laptop.architect-2` on #1153
  (https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152,
  2026-10-08 03:02 UTC).
