- **STORED BENCH (#1547, 2026-10-07)** The cargo feature `sim` of `home`, off by
  default, adds `#[doc(hidden)] pub mod bench`: `entry` calls `stored::entry`, and
  `read` calls `stored::read` and gives each series' channel, type, and bytes. Only the
  bench `benches/stored.rs` (`test = true`) uses it. `read` gives all three fields, so
  the compiler cannot skip a decode that production does, and the bench passes each item
  to `divan::black_box`. Run it with `cargo bench -p home --bench stored`. Lost: a copy
  of `stored` in the bench through `#[path]`, which breaks at its first `crate::` item,
  and a time of `Shard` writes and reads, which hides the cost of the body in the cost
  of the write. Decided by `laptop.architect` (2026-10-07T18:46:13Z):
  https://github.com/synnaxlabs/foundation/issues/1547#issuecomment-6044535576.
  `cargo bench -p home` turns on `sim` through a dev-dependency of `home` on itself,
  since the bench host runs no features. Decided by `laptop.architect`
  (2026-10-07T19:05:26Z):
  https://github.com/synnaxlabs/foundation/issues/1547#issuecomment-6044862850. Amended
  by `laptop.architect` (2026-10-08T01:01:28Z):
  https://github.com/synnaxlabs/foundation/pull/1568#issuecomment-6049989224. The
  feature is `sim`, not `bench`, since the feature says that the module is test-only,
  and the module keeps the name `bench`, since it says what the module serves.
  Supersedes the feature name of
  https://github.com/synnaxlabs/foundation/issues/1547#issuecomment-6044535576 and
  https://github.com/synnaxlabs/foundation/issues/1547#issuecomment-6044862850.
