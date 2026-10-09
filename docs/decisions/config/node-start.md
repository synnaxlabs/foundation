- **NODE START (2026-10-08)** `foundation start` runs a node on a data directory until
  SIGINT or SIGTERM, then exits 0. `ops` cannot depend on `node`, so `start` is a
  `Command` variant beside `mcp`, not an entry of the operation table: like `mcp`, it
  runs the process and is not an operation of a node. The help names it, and `foundation
  docs` does not. `ops::cli` writes nothing for it and gives `Run::Start(Start { data,
  json, name })` to `main` in `node`. `--data` is `foundation-data` in the working
  directory by default, so `cli` reads no environment. `start` has no MCP tool: MCP acts
  on a running node. The first start on a data directory needs `--name`, and a later one
  reads the name there (NODE NAME). Once the node has claimed the data directory, a
  thread of `main` writes `Start::line`: `node edge runs in foundation-data. Stop it
  with Ctrl-C.`, or `{"name":"edge","data":"foundation-data"}` with `--json`. `data` is
  the path as given, and a path that is not UTF-8 is written lossily, as `Path::display`
  does. A write that fails changes nothing. A node that stops before the thread writes
  can exit with no line. `Start::fail` writes an `ops::Failure { code, message, fix }`
  as `cli` writes its own errors, and gives exit status 1. So `ops` keeps the one output
  form, and `main` gives the facts. The codes: `node.busy`, `node.data`, `node.unnamed`,
  `node.renamed`, `node.name`, and `node.failed`. `node.data` is "this user cannot write
  the data directory": `os::Error::Dir`, and each `env::files::Error::Io` whose code is
  `EACCES`, `EPERM`, or `EROFS`, in `node::Error::Directory` or in the
  `buffer::Error::Files` of `node::Error::Buffer`. Its message is "cannot write the data
  directory {data}: {error}", and its fix "Let this user make and write {data} and each
  file in it, or give another directory with `--data`". Each other `Directory` error but
  `Busy` is `node.failed`. `Node::stopper` gives a `Stopper` that stops the node from
  another thread. A stop after the node ended does nothing. Lost: a line that `node`
  writes itself, a second owner of the output form; an entry of the table with a flag
  that each of its four users skips. Decided by `laptop.architect-2`
  (2026-10-08T02:21:16Z,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6050855867;
  2026-10-08T05:00:46Z, `--name` and the line,
  https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6052649826;
  2026-10-09T18:30:45Z, `running` (now `line`), `fail`, `Failure`, and the `Stopper`
  doc, https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6086905545;
  2026-10-09T19:31:23Z, `start` as a `Command` variant, which amends the first, and the
  line, https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6087849613).
  `main` writes the line on a thread of its own, so a standard output that nobody reads
  blocks neither shard 0 nor the stop. `main` drops the handles of that thread and of
  the thread that waits for the signal, since each waits only on the process
  (`env::thread::Handle`). Decided by `laptop.architect-2` (2026-10-09T22:02:37Z,
  https://github.com/synnaxlabs/foundation/pull/2188#issuecomment-6089996352;
  2026-10-09T22:10:27Z, the stop handle also after a clean end,
  https://github.com/synnaxlabs/foundation/pull/2188#issuecomment-6090093334;
  2026-10-09T22:24:30Z, `Start::line` in place of `Start::running`,
  https://github.com/synnaxlabs/foundation/pull/2188#issuecomment-6090256620;
  2026-10-09T22:36:49Z, `node.data` by its cause, which amends item 2 of 6086905545,
  https://github.com/synnaxlabs/foundation/pull/2188#issuecomment-6090417728).
  `--listen`, and the files `address` and `admin.key`, come with #1744
  (https://github.com/synnaxlabs/foundation/issues/1732#issuecomment-6053227526).
