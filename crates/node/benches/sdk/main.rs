//! The time of a write of 2 values through `sdk` and through `program`, against a node
//! process on loopback, of the two legs that `sdk` adds between the caller and the
//! client's thread, and of a write and the read of its frame. Run with
//! `cargo bench -p node --bench sdk`.
//!
//! Each line gives the p50 and p99 of the calls of each round, in µs:
//!
//! - `leg to task`: from a `try_send` on a Tokio channel of 1 to the return of the
//!   `recv` of a task on an `os` shard, as `send` in `sdk/rust/src/lib.rs` sends each
//!   call to the task of `served` in `sdk/rust/src/write.rs`.
//! - `leg to caller`: from the task's `try_send` on a `std` `sync_channel` of 1 to the
//!   return of the caller's `recv`, as `served` sends each outcome to `wait`.
//! - `sdk`: `sdk::Writer::write` of 2 values with the node's stamp.
//! - `program`: `program::Writer::draft` and `write` of the same frame, on an `os`
//!   shard of its own, as `served` writes it.
//! - `sdk read`: an `sdk` write, then `sdk::Reader::next` of its frame, with a latest
//!   reader of the same client, and `series` and `lost` of each place.
//! - `program read`: a `program` write, then `program::Reader::next` of its frame, as
//!   the task of `sdk/rust/src/read.rs` reads it.
//! - `sdk read twice`: `sdk read` with a reader that names `plant.a` twice.
//! - `sdk next held`: `sdk::Reader::next`, and `series` and `lost` of each place,
//!   after an `sdk` write and a spin of 1 ms, when the client's thread holds the
//!   frame: the two legs and the work of the task, with no wait for the node. Its
//!   rounds are of 500 calls.
//!
//! The channels are those of `Writer::open`, and each shard is one of `Client::start`
//! in `sdk/rust/src/lib.rs`: `os`, with the caller's log. Before each call of the legs
//! and of the `sdk` lines, the caller spins for [`GAP`], so that the task parks before
//! each send, as it does between the calls of a program.
//!
//! Each write must give `Taken`, and a frame past the ring of the home shard is
//! `Lost`. A ring holds more writes when several share a commit, which depends on the
//! host: a CI runner lost frame 10,795 of two lines on one node. So each line writes
//! its 5,500 frames on a node of its own, with twice the disk budget of the node tests,
//! where the ring holds 12,500 writes of a commit each (`sdk next held` on box1), also
//! under `cargo test`, so that a test fails when they do not fit.
//! Judge `sdk` against `program` from one run, and each leg against its p50: a p99 far
//! over it is a wait for a wake.
//!
//! Only Unix: the rig stops its node with SIGTERM.

/// Only Unix: the rounds and the table of the bench.
#[cfg(all(unix, not(loom)))]
#[expect(clippy::print_stdout, reason = "a bench prints its table")]
#[expect(
    clippy::disallowed_methods,
    reason = "a benchmark reads a real clock and blocks on its threads"
)]
mod bench;
/// Only Unix: the channels of the bench, and the session of its `program` line.
#[cfg(all(unix, not(loom)))]
#[expect(
    dead_code,
    clippy::unwrap_used,
    clippy::missing_assert_message,
    reason = "a helper of the node tests fails loud, and the bench uses only some"
)]
#[path = "../../tests/it/plant.rs"]
mod plant;
/// Only Unix: the helpers of the node tests that start and stop a node.
#[cfg(all(unix, not(loom)))]
#[expect(
    dead_code,
    clippy::missing_assert_message,
    reason = "the bench uses only some of the helpers of the node tests"
)]
#[path = "../../tests/it/rig.rs"]
mod rig;
/// Only Unix: the output of `foundation status --json`, which the rig reads.
#[cfg(all(unix, not(loom)))]
#[expect(dead_code, reason = "the bench uses only some of the helpers")]
#[path = "../../tests/it/status.rs"]
mod status;

/// Only Unix: runs the bench.
#[cfg(all(unix, not(loom)))]
fn main() {
    bench::run();
}

#[cfg(not(all(unix, not(loom))))]
fn main() {}

/// Only Unix: `bytes` as UTF-8, for the helpers of the node tests.
#[cfg(all(unix, not(loom)))]
fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("UTF-8")
}
