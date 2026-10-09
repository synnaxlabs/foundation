- **SIM FILE CLOSES (2026-10-09)** `sim::node::Node::file_closes(&self) -> Vec<PathBuf>`
  gives the path of each file descriptor that the node closed or dropped, in order,
  since the run started: the path of its open, or of the last rename that it made, with
  only its names. A rename that ends after its future dropped counts, when it ends
  before the close. A file removed while open still gives that path. A crash closes each
  descriptor of the node, a leaked one too. Those that the drops of its futures close
  come first. The leaked ones come last, in the order that their opens started. An open
  whose future dropped made no descriptor and adds nothing. A test asserts from it the
  order of two closes in one poll, which no probe can see. Lost: a close stamp per path,
  since two closes in one poll have the same instant (#1835, decided by
  `laptop.architect-2`, 2026-10-09 04:51 UTC:
  https://github.com/synnaxlabs/foundation/issues/1835#issuecomment-6074501894; a crash
  of a held descriptor, 04:58 UTC:
  https://github.com/synnaxlabs/foundation/issues/1835#issuecomment-6074586228; the
  normal form, 05:01 UTC:
  https://github.com/synnaxlabs/foundation/pull/2110#issuecomment-6074620722; the leaked
  and renamed cases, 05:12 UTC:
  https://github.com/synnaxlabs/foundation/pull/2110#issuecomment-6074738970; the path
  that a rename of a descriptor sets and that a call of it names, 06:16 UTC:
  https://github.com/synnaxlabs/foundation/pull/2110#issuecomment-6075502445). Also
  lost, in the plan: a held close, since a drop closes at once and cannot wait
  (https://github.com/synnaxlabs/foundation/issues/1835#issuecomment-6074488621).
