- **SDK WRITER** `sdk::Client::open_writer(write::Config)` opens a `sdk::write::Writer`
  at the client's node, which must be the home of the index of its channels (BQ12, until
  #3104). `write::Config::new(channels)` gives authority 0 and no lease, so a writer of
  a higher authority, such as a connector, takes control from it. The client's thread
  opens a `program::Writer` in a task of its own, which serves the calls of its writer
  until the writer closes or drops, or the client ends. Each call copies its samples
  into a frame that the writer keeps, sends it to the task on a channel of 1, and
  blocks on a `std` channel of 1 for the outcome and the frame. So a warm write makes
  no allocation on the caller's thread (`crates/node/tests/it/sdk.rs`, with
  `counting::Local`), and no draft is held across calls. The client's thread makes the
  ACK of each frame in `record_sent_frame` of noq, about 1.1 to 1.2 allocations a write
  (a `program::Writer` alone makes about 1.04; a sampled count on box2, Xeon 8488C,
  before 706b6689b), until #3281; `sdk` makes no other.
  `Writer` is `Send` and holds no borrow of `Client`. The plan
  (https://github.com/synnaxlabs/foundation/issues/3266#issuecomment-6255057555) was
  approved by `laptop.architect-2` at 2026-10-18T21:04:06Z
  (https://github.com/synnaxlabs/foundation/issues/3266#issuecomment-6255123001), with
  these terms:
  1. `Error::Stream(program::Error)`: the node refused the open of a stream, or a call
     of a writer failed, and the session goes on. A call gives `Error::Session` with the
     cause only when the session ended. `Error::Closed` comes only after
     `Client::close` or a drop of the client.
  2. A `Value::Stamp` at the index place is the stamp of the frame, sent with
     `Time::Frame(Path::Live)`. With no value there, the frame goes with `Time::Node`.
  3. `write` and `write_series` take the places in any order. `sdk` sorts them in the
     kept frame. A place given twice gives `Stream(Layout)`.
  4. A place past the places gives `Stream(Layout(OutOfRange))`, so `Error::Value {
     place, scalar, data_type }` means only a value whose type is not the type of its
     place.
  5. Superseded by the place order below.
  6. One request channel into the client's thread, which `Client` holds next to the
     `Mutex` of its thread. Each send is a `try_send` or an unbounded send, so no send
     blocks or panics in an async context.
  Lost: a borrowed `Writer<'_>` of `Client`, because a PyO3 class holds no borrow, so a
  Python close could not end a session with a live writer.
  Place order: place `n` of `write` and `write_series` is `Config::channels[n]`, and
  the index is at its own place when `channels` names it, else at place
  `channels.len()`. `Writer::places` gives the places in that order, and
  `Writer::index` gives the place of the index. Lost: the stamp as an argument of each
  call (a caller who names the index gets a new error at the open, and each call of the
  common case carries a `None`). Decided by `laptop.architect-2` on
  2026-10-18T22:10:54Z
  (https://github.com/synnaxlabs/foundation/pull/3275#issuecomment-6255774922, items 1
  and 2) and held on 2026-10-18T22:20:44Z
  (https://github.com/synnaxlabs/foundation/pull/3275#issuecomment-6255869839, item
  2). It supersedes item 5.
  A panic of the client's thread: a build where panics unwind can have one (the
  `wheel` profile of `sdk-python`). `Client` sets a shared flag at its close or drop,
  before its thread ends. A call of a writer whose task is lost gives `Closed` when the
  flag is set, else `Panicked`. Term 1 holds. Decided by `laptop.architect-2` on
  2026-10-18T22:20:44Z
  (https://github.com/synnaxlabs/foundation/pull/3275#issuecomment-6255869839, item
  3). `Error::Stream` states the errors that leave the writer going (`Layout`,
  `Unsent`, and `Pool`, as `program` leaves it going) and those that end it and come
  again at each call (item 5 of that comment, and
  https://github.com/synnaxlabs/foundation/pull/3275#issuecomment-6255939595).
  `program::Refusal` is the stop code that `program::Error::Refused` holds, at the root
  of `program`, as `Place` is; `program::write::Refusal` is the outcome of one frame.
  No `sdk::Refusal` until PR 7 of #2177. Decided by `laptop.architect` on
  2026-10-18T22:40:21Z
  (https://github.com/synnaxlabs/foundation/pull/3275#issuecomment-6256055880, item
  2).
  The node stamps a frame with no index value at mesh time now, or 1 ns after the last
  stamp that it gave the writer. So after a stamp of the caller ahead of now, a frame
  that the node stamps can get `Refusal::Order` (#3282). Each writer holds a stream of
  the session, and the node bounds the streams of a session (64 by default, one of
  them the hello): past that bound, an open waits until another stream of the session
  ends, such as at the close or drop of a writer.
  `crates/node/benches/sdk/` gives the p50 and p99 of each round of
  `sdk::Writer::write` and of `program::Writer` with 2 values, and of each leg between
  the caller and the writer task, with the task parked before each call. Decided by
  `laptop.architect-2` on 2026-10-18T22:28:03Z
  (https://github.com/synnaxlabs/foundation/pull/3275#issuecomment-6255939595). On a
  quiet host at 07f84b0cd
  (https://github.com/synnaxlabs/foundation/pull/3275#issuecomment-6256170281), the
  leg to the task, which is on the path of a latest-mode read, takes 7.8 µs p50 and
  8.9 µs p99, over the P1 limit of 2.5 µs. The leg to the caller takes 3.7 µs p50 and
  8.6 µs p99, and a write takes 39.8 µs p50, 12.5 µs more than `program`. The merge of
  `main` in 706b6689b changes the write path of `program`, so the numbers of a write are
  of 07f84b0cd only; the code of each leg is the same by instructions at x86-64-v2 after
  it (https://github.com/synnaxlabs/foundation/pull/3275#issuecomment-6256773581, item
  10), also at 104f87783
  (https://github.com/synnaxlabs/foundation/pull/3275#issuecomment-6257299140, finding
  5). Accepted as the price of SDK CLIENT. No spin goes before the park, because the p99
  of the leg is inside its p50 plus the noise of the rounds. Decided by
  `laptop.architect-2` on 2026-10-18T23:01:32Z
  (https://github.com/synnaxlabs/foundation/pull/3275#issuecomment-6256253289), and
  confirmed on this text on 2026-10-19T00:27:25Z
  (https://github.com/synnaxlabs/foundation/pull/3275#issuecomment-6257075310).
  `Writer::bytes_max` gives the most bytes of one frame of the writer, its head
  included: the largest block of the session's pool of frames, which the client's
  thread sends with the places at the open. `write_series` refuses series whose bytes
  go over it before its first copy into the kept frame, with
  `Stream(Pool(TooLarge))`, `requested` the bytes of the series and `largest` the
  value of `bytes_max`. So the kept frame never grows past it. The draft still checks
  the exact frame. Decided by `laptop.architect-2` on 2026-10-19T12:51:02Z
  (https://github.com/synnaxlabs/foundation/pull/3325#issuecomment-6267629281), for the
  round 3 defect of PR 3325: before, `write_series` copied series of any size into the
  kept frame before the draft refused them. Lost: a fill closure on `write_series`
  that copies from the caller straight into the kept frame, as the binding then needs
  a fill and a send as two steps.
