- **SIM FILE CLOSES (2026-10-09)** `sim::node::Node::file_closes(&self) ->
  Vec<PathBuf>` gives the path of each file descriptor that the node closed or
  dropped, in order, since the run started: the path of its open, or of its last
  rename, with only its names. A file removed while open still gives that path. A
  crash drops each descriptor that a task of the node holds. An open whose future
  dropped made no descriptor and adds nothing. A test asserts from it the order of two
  closes in one poll, which no probe can see: at the stop of a node with a region, the
  log of the mesh closes before `lock` (#1835). Lost: a close stamp per path, since
  two closes in one poll have the same instant; and a held close, since a drop closes
  at once and cannot wait (#1835, decided by `laptop.architect-2`, 2026-10-09
  04:51:11Z:
  https://github.com/synnaxlabs/foundation/issues/1835#issuecomment-6074501894; the
  crash sentence, 2026-10-09 04:58:16Z:
  https://github.com/synnaxlabs/foundation/issues/1835#issuecomment-6074586228).
