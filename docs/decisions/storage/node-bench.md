- **NODE BENCH (#1637, 2026-10-07)** The cargo feature `sim` of `node`, off by default
  (`node`'s dev-dependency on itself turns it on for the bench), adds `#[doc(hidden)]
  pub mod bench` with `Scope { new, spawn }` and its `Default` over `scope::Scope`. Only
  the bench `benches/scope.rs` (`test = true`) uses it. Its `env::tasks::Driver` keeps
  each task, `spawn` gives it, and the bench polls it by hand, so a time holds only
  `Spawned::poll` and the future's poll. A `bare` line polls the boxed future directly
  in the same binary, as the control. Lost: a time through `Node::spawn` on `sim` or
  Tokio, which hides a 0.3 ns change in the executor's cost, and a copy of the poll
  before `clone_from`, which #1627 decided and the `bare` control replaces. The
  `same_waker` time is the check on `clone_from` until #715 gates it with a baseline
  from the form with `clone_from`: an `Arc` waker clone allocates nothing, so no
  allocation count can. Decided by `laptop.architect-2` (2026-10-07 23:56 UTC):
  https://github.com/synnaxlabs/foundation/issues/1637#issuecomment-6049244976; the
  surface of `spawn`, in round 1 of #1666 (2026-10-08 00:11 UTC):
  https://github.com/synnaxlabs/foundation/pull/1666#issuecomment-6049426380. The
  bench's `Driver`, beside those of `os` and `sim`, is the #1632 clause, by
  `laptop.architect-2` (2026-10-08 00:18 UTC):
  https://github.com/synnaxlabs/foundation/issues/1632#issuecomment-6049501422. #1632
  applies its text to the doc of `env::tasks::Driver` and to ENV SEAMS. The feature is
  `sim`, since a hook that only a bench or a fuzz target uses is test-only (#1570), by
  `laptop.architect-2` (2026-10-08 00:56 UTC):
  https://github.com/synnaxlabs/foundation/pull/1666#issuecomment-6049939448. It
  supersedes
  https://github.com/synnaxlabs/foundation/issues/1637#issuecomment-6049244976 in its
  clause that the feature is `bench`.
