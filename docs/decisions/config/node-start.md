- **NODE START (2026-10-08)** `foundation start` runs a node on a data directory until
  SIGINT or SIGTERM, then exits 0. `ops` cannot depend on `node`, so `start` is an
  entry of the operation table, and the help names it. `ops::cli` writes nothing for
  it and gives `Run::Start(Start { data, json, name })` to `main` in `node`. `--data`
  is `foundation-data` in the working directory by default, so `cli` reads no
  environment. `start` has no MCP tool: MCP acts on a running node. The first start on
  a data directory needs `--name`, and a later one reads the name there (NODE NAME).
  When the node runs, `Start::running` writes `node edge runs in foundation-data. Stop
  it with Ctrl-C.`, or `{"data":"foundation-data","name":"edge"}` with `--json`. A
  write that fails changes nothing. `Start::fail` writes an `ops::Failure { code,
  message, fix }` as `cli` writes its own errors, and gives exit status 1. So `ops`
  keeps the one output form, and `main` gives the facts. The codes: `node.busy`,
  `node.data`, `node.unnamed`, `node.renamed`, `node.name`, and `node.failed`.
  `Node::stopper` gives a `Stopper` that stops the node from another thread. A stop
  after the node ended does nothing. Lost: a line that `node` writes itself, a second
  owner of the output form. Decided by `laptop.architect-2` (2026-10-08T02:21:16Z,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6050855867;
  2026-10-08T05:00:46Z, `--name` and the line,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6052649826;
  2026-10-09T18:30:45Z, `running`, `fail`, `Failure`, and the `Stopper` doc,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6086905545).
  `--listen`, and the files `address` and `admin.key`, come with #1744
  (https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6053227526).
