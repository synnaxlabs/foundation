- **CLIENT HELLO (2026-10-08)** A program's session with the node it connects to
  has a hello stream, request streams, and read streams (`wire::hub::client`, kinds 4,
  5, and 6). Hub
  kinds are one space: the first byte of a hub stream picks its decoder
  (`laptop.architect`, 2026-10-08T10:19:16Z,
  https://github.com/synnaxlabs/foundation/pull/1854#issuecomment-6057726181). The
  hello stream is the first hub stream and lives as long as the session. The node sends
  a `Challenge` (a fresh nonce and its mesh time) first and after each admitted hello;
  the program sends a `Signed` hello that echoes it, first and to renew at half its
  life. The wire hello is kind 4, the signed bytes of the hello with no tag
  (`Hello::encode`), and the signature, so an SDK writes one form (`laptop.architect`,
  2026-10-08T10:15:03Z,
  https://github.com/synnaxlabs/foundation/pull/1854#issuecomment-6057658762). Each
  refusal on the hello stream ends the session. A request stream carries one
  `Request` and its body, then one `Response` and its body; each body is a run of at
  most `BODY_BYTES_MAX` (16 MiB), which the SDK checks before it sends. A body over the
  cap at the node gets `MALFORMED`. `access::Rules::renew` checks a renewal as `admit`
  checks a first hello, after `Error::Changed` for another subject, key, `via`, or
  connection, and takes the carrier from the `Admitted`: a move to another node is a
  new connection. `Changed` names the first field that differs, an
  `access::proof::Field` (`laptop.architect`, 2026-10-08T10:19:16Z,
  https://github.com/synnaxlabs/foundation/pull/1854#issuecomment-6057726181).
  Stop codes: `REFUSED` 20 for an unknown subject, an unlisted key, or
  a bad signature, which tell about the spec and so share one code (`Signature` too:
  `admit` checks it only for a listed key); each other step its own code, `UNSYNCED`
  21, `STALE` 22, `VIA` 23, `EXPIRED` 24, `CHANGED` 26. Code 25 was `CAPPED` and stays
  unused (`laptop.architect`, 2026-10-09T03:15:58Z,
  https://github.com/synnaxlabs/foundation/pull/2088#issuecomment-6073528599).
  Supersedes `CAPPED` 25 of
  https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6057298526. `hub`
  maps each `access::proof::Error` to its `Refusal` in one exhaustive `match`, and
  `serve` returns the exact error for the node's log.
  `connection::Key` writes as UUID text in byte order.
  Lost: one `REFUSED` for each refusal, which hides an unsynced node from a program;
  and a verify before the spec lookup, so that each refusal costs the same, which costs
  a verify for each hello from an unknown client. Decided by `laptop.architect` at
  2026-10-08T09:53:58Z
  (https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6057298526),
  which extends the refusal ruling of #1744
  (https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6053329389).
  Each side knows the kind of each stream, so it calls the `decode` of the message that
  it expects (`Challenge`, `Signed`, `Request`, `Response`), each of which refuses
  another kind with `Error::Kind`; a request or response gives its `Body`, which counts
  the body's messages (`laptop.architect`, 2026-10-08T15:38:46Z,
  https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6063499704). When
  the stream ends, `Body::end` gives `Error::Unfinished` if bytes of the body remain, so
  each rule of a body is in `wire` (`laptop.architect`, 2026-10-08T16:52:55Z,
  https://github.com/synnaxlabs/foundation/pull/1918#issuecomment-6064815697). Lost:
  a decoder for each side that takes the kind of the stream from its first message,
  because each caller checks the kind again; `Gateway::hello()` and
  `Gateway::request()`, the kind at construction, because each caller still matches
  variants that its stream cannot carry; and `Gateway` and `Program` for request
  streams only, a header-or-body enum where the caller knows which comes.
  Supersedes shape decision 1 of #1854
  (https://github.com/synnaxlabs/foundation/pull/1854#issuecomment-6057726181), one
  decoder for each side that takes the kind of the stream from its first message.
  Amended: a third kind of stream, a read stream, carries one reader session. The
  program sends a `Read` (kind 6: the open's length and the signature) and then the
  `client::Open` as a body of at most `OPEN_BYTES_MAX` (64 KiB): the mode (latest 1,
  complete 2), then each pattern of the selector as written, each a `u16` length and its
  text. The selector matches channels of one index, and at most `PLACES_MAX` (65,536)
  places with their index; else `MALFORMED`, before the reader opens. A named reader
  keeps no cap, and the program refuses a `Places` over the cap with `Error::Places`
  (`laptop.architect`, 2026-10-16T06:47:02Z,
  https://github.com/synnaxlabs/foundation/issues/2808#issuecomment-6209795527). The
  node gives `serve::Error::Places` for a match over the cap. A `Start` at the cap with
  names of 255 bytes takes at most 26 MB at the program (counting allocator, box1)
  (`laptop.architect`, 2026-10-16T08:11:58Z,
  https://github.com/synnaxlabs/foundation/pull/2847#issuecomment-6211097743). The
  node sends `Places` (kind 6: the count of places and the place of the index), a run of
  `Place` (key, sample type as the text that `types::sample::Type` writes and reads,
  name) (`laptop.architect`, 2026-10-10T23:52:16Z,
  https://github.com/synnaxlabs/foundation/pull/2327#issuecomment-6103487682), then
  the replies of HUB WIRE from `Opened`. As in HUB WIRE, a message of the run holds
  whole places (box1.builder-2, 2026-10-10T23:26:58Z,
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6103306402,
  approved by `laptop.architect` at 2026-10-10T23:31:47Z in the ruling below), and no
  more than remain (`laptop.architect`, 2026-10-11T00:45:04Z,
  https://github.com/synnaxlabs/foundation/issues/2332#issuecomment-6103881921);
  `client::place::messages` cuts the run into messages, so the sender writes no rule of
  the run (`laptop.architect`, 2026-10-11T01:55:44Z,
  https://github.com/synnaxlabs/foundation/pull/2327#issuecomment-6104388686); and a
  latest read needs a stream of class `Latest`, and a complete read one of class
  `Complete`, else `MALFORMED` (box1.builder-2, 2026-10-10T23:26:58Z,
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6103306402, approved
  by `laptop.architect` at 2026-10-10T23:31:47Z in the ruling below). At the program,
  `client::Reader` decodes the start of a read stream: `Places`, then the run. With the
  last place it gives the `Start`: the places, their key set, and the `hub::Reader` of
  their count and the read's mode, with which the program decodes each reply. Only
  `wire` builds that reader, so the program side builds no `Open` with a grant that it
  never sends. `client::Reader` refuses an
  index place whose sample type is not `Stamp` with `Error::Unstamped`
  (`laptop.architect`, 2026-10-11T00:45:04Z,
  https://github.com/synnaxlabs/foundation/issues/2332#issuecomment-6103881921). It
  gives `Error::Finished` for a stream that the node finishes before the last place
  (`laptop.architect`, 2026-10-16T07:20:38Z,
  https://github.com/synnaxlabs/foundation/pull/2839#issuecomment-6210285214), and a
  call of `client::Reader` after that place panics (`laptop.architect`,
  2026-10-11T07:53:39Z,
  https://github.com/synnaxlabs/foundation/pull/2368#issuecomment-6106869729, and
  2026-10-16T06:47:02Z,
  https://github.com/synnaxlabs/foundation/issues/2808#issuecomment-6209795527).
  Supersedes the caller's `Error::Finished` of
  https://github.com/synnaxlabs/foundation/pull/2368#issuecomment-6106869729. Lost:
  one decoder for the whole stream. Its copy of each reply cost the program 6.9 ns for
  each message (644.7 against 188.6 ns for a frame of 66 messages). `#[inline]` on
  `hub::Reader::decode` removed that cost, and cost the node 0.31 ns for each message
  (188.6 to 209.0 ns) and 5.1 ns for a head (8.7 to 13.8 ns), on box1 under load
  (https://github.com/synnaxlabs/foundation/pull/2368#issuecomment-6106494175;
  `laptop.architect`, 2026-10-11T07:53:39Z,
  https://github.com/synnaxlabs/foundation/pull/2368#issuecomment-6106869729).
  Supersedes `FromNode::Home`, `body`, `end`, and item 1 of
  https://github.com/synnaxlabs/foundation/issues/2332#issuecomment-6103881921.
  `client::Places::decode` and `client::place::decode` are public, beside the public
  encode of each, so that the fuzz target `wire_hub_client` checks that each message
  encodes to its bytes. One decoder checks the run and builds the key set
  (`laptop.architect`, 2026-10-16T06:57:29Z,
  https://github.com/synnaxlabs/foundation/issues/2808#issuecomment-6209940652):
  `client::write::Opening`, which `client::Reader` holds (`laptop.architect`,
  2026-10-18T03:22:04Z,
  https://github.com/synnaxlabs/foundation/pull/3112#issuecomment-6245142760).
  Supersedes
  https://github.com/synnaxlabs/foundation/issues/2332#issuecomment-6104390669. The
  program sends no credit: the stream's flow control bounds the frames in flight. The
  program knows the kind of each stream that it opens; the node knows the kind of the
  hello stream only, and decodes the first message of each later stream with
  `client::First::decode`, which gives a `Request`, a `Read`, or a `Write` (`Write`:
  `laptop.architect`, 2026-10-18T02:34:55Z,
  https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6244779011) and
  refuses each other kind with `Error::Kind`. The lost reason above, that each caller
  checks the kind again, does not apply: each variant comes, and the node handles each.
  The node serves the write stream (HUB CLIENT, amended by #2718).
  Supersedes, for the node, the rule that each side knows the kind of each stream, of
  https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6063499704.
  `client::Refusal` gains `UNKNOWN` 16 (a selector that matches no channel), `NOT_HOME`
  17, and `FAILED` 18. Lost: a role byte in `Request`, a change of a form that each SDK
  writes; and the stream class as the kind of the stream, because a request stream is of
  class `Complete` and a class sets priority. Decided by `laptop.architect`
  (2026-10-10T23:31:47Z):
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6103341608. A text
  field of the client wire that does not read keeps its cause. `Error::Subject`,
  `Error::Selector`, and `Error::Name` hold a `text::Error<Box<name::Error>>`, and
  `Error::Sample` holds a `text::Error<sample::Error>` (`laptop.director`,
  2026-10-11T00:48:05Z,
  https://github.com/synnaxlabs/foundation/pull/2327#issuecomment-6103904407; shape
  approved by `laptop.architect`, 2026-10-11T01:55:44Z,
  https://github.com/synnaxlabs/foundation/pull/2327#issuecomment-6104388686). The
  errors of the client wire are `wire::hub::client::Error`, and `text` is
  `wire::hub::client::text`. `wire::hub::Error` holds the errors of the home wire
  and of the replies of a write stream (`laptop.architect`, 2026-10-18T03:22:04Z,
  https://github.com/synnaxlabs/foundation/pull/3112#issuecomment-6245142760), holds no
  `Box`, and is `Copy`, with a check at compile time. A variant that decoders of both
  wires give (`Empty`, `Kind`, `Length`, `Body`, `Unfinished`) is in each type, with the
  same fields and message. `Run` keeps one message, and the doc of each names its own
  items. `wire::hub::client::Error::Finished` is "the node finished the stream before
  its last place", which `wire::hub::client::Reader::decode` and
  `wire::hub::client::write::Opening::decode` give at the end of the stream;
  `hub::Error::Finished` stays the home's. No variant of `wire::hub::client::Error`
  holds a `hub::Error`: the program gets the errors of the replies from the
  `hub::Reader` that `wire::hub::client::Reader` gives.
  `program::Error::Message` (the name, `laptop.architect`, 2026-10-15T18:23:47Z,
  https://github.com/synnaxlabs/foundation/pull/2724#issuecomment-6199257488) holds a
  `wire::hub::client::Error`; `hub::serve::Error::Client` holds one, code `MALFORMED`,
  and `serve::Error::Message` keeps the errors from a reader's node. Lost: a variant
  `wire::hub::client::Error::Hub(hub::Error)`, which brings back two levels of error and
  puts each variant that only a frame decoder gives in the type of the client wire. Why:
  a `hub::Error` that is not `Copy` cost each frame that a node or a program decodes,
  and bought nothing, because each text error keeps its cause in both forms. Against the
  `Copy` form, `main` gave +0.78 ns on `decode_ends/3` (each pass apart). It gave
  +0.59 ns on `decode_a_frame/1` and +0.34 ns on `decode_a_frame/3` (the passes of the
  two commits overlap on both). Each is of three passes of each commit on
  c7i.metal-24xl (Xeon Platinum 8488C, CPU 2)
  (https://github.com/synnaxlabs/foundation/issues/2377#issuecomment-6106581972).
  The split, at 7000b19ba, then ran against its parent d7a67a52e, with the flags of
  BENCH BASELINES. Each bench got three passes of each commit on one c7i.metal-24xl
  (Xeon Platinum 8488C, CPU 2). Each result is the median of the head passes against
  that of the base passes. In `wire` `hub`
  (https://github.com/synnaxlabs/foundation/pull/2392#issuecomment-6108713214),
  `decode_ends/3` gave -0.55 ns (4.257 to 4.292 against 4.41 to 4.87 ns, passes apart).
  The passes of each other line overlap: `decode_a_body/1` -0.53 ns, `/8` -0.02 ns,
  `/64` -2.1 ns; `decode_a_credit` 0.00 ns; `decode_a_frame/1` -0.07 ns, `/3` +0.23 ns,
  `/1000` +0.2 ns, `/100000` 0.00 µs; `decode_ends/1` +0.02 ns, `/1000` +1.4 ns,
  `/100000` +0.01 µs; `encode_and_decode_a_head` +0.23 ns; `encode_ends/1` 0.00 ns, `/3`
  0.00 ns, `/1000` -0.1 ns, `/100000` +0.02 µs; `encode_ends_by_place/1` +0.02 ns, `/3`
  +0.01 ns, `/1000` 0.00 µs, `/100000` -1.6 µs. In `hub` `serve`
  (https://github.com/synnaxlabs/foundation/pull/2392#issuecomment-6108750674), by p50
  in ns per frame, `narrow 64k` gave -36 (2288 to 2296 against 2307 to 2331, passes
  apart), and `timer` gave +1 (15 against 14 in each pass, passes apart). The passes of
  each other line overlap: `narrow 1472` -42, `wide 1472` +2, `complete 1472` -8,
  `complete half 1472` -6, `one set 1472` -20, `two sets 1472` -18, `wide 64k` -11. Each
  line keeps its allocations. `timer` times an empty closure, so no line that times
  product code is slower with its passes apart. `laptop.architect` accepted each line
  that is slower by the median (2026-10-11T12:22:17Z,
  https://github.com/synnaxlabs/foundation/pull/2392#issuecomment-6108970867).
  Decided by `laptop.architect` (2026-10-11T08:26:42Z,
  https://github.com/synnaxlabs/foundation/issues/2377#issuecomment-6107119794;
  surface approved 2026-10-11T10:16:56Z,
  https://github.com/synnaxlabs/foundation/issues/2377#issuecomment-6107983027).
  Supersedes item 2 (one `wire::hub::Error`, and no `wire::hub::client::Error`) and,
  for `wire::hub::Error`, the "not `Copy`" of Text fix 3 and of the Public surface
  line, of https://github.com/synnaxlabs/foundation/pull/2327#issuecomment-6104388686,
  and item 4 ("remove `Copy`") of
  https://github.com/synnaxlabs/foundation/pull/2327#issuecomment-6103904407.

  Amended (#3283): `Places` holds only the count of places, and each `Place` holds the
  place of the index of its channel's group after the key. The rules are in HUB
  CLIENT, Amended (#3283).
