- **HUB LINK (2026-10-08)** `Hub::link(session)` gives a `hub::Link` for one transport
  session. `node` calls `Link::serve` for each hub stream that NODE PORT serves once it
  reads its header. On a client link, the first stream given to `serve` is its hello
  stream, and each later one a request stream or a read stream, by its first message
  (`client::First::decode`); `serve` takes the role at the call (`laptop.architect`,
  2026-10-08T18:56:15Z,
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066908418; the read
  stream by `laptop.architect`, 2026-10-10T23:31:47Z,
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6103341608).
  `hub::Config` gets `node` (the `via` that `admit` checks), `time` (`clock::Reader`),
  and `entropy` (the nonces). The link waits for `Admitted::ends` of a hello with
  `clock::Reader::reach` (CLOCK REACH), so `hub` knows nothing of how mesh time moves
  against the monotonic clock, and gets no second clock. A link has one open request:
  it frees the request when `Reply::send` is called or the `Reply` drops, before the
  first byte of the response, so a client that sends its next request when a reply
  ends never gets `MALFORMED`. `Reply::send` panics on a body over `BODY_BYTES_MAX`, a
  precondition that the maker of the body checks. Each order error names its cause,
  though both stop with `MALFORMED`: `serve::Error::Unadmitted` (a request or read
  stream before an admitted hello) and `Pending` (a request while one waits for its
  reply).
  A message of the wrong kind for its stream, such as a hello on a request stream, is
  `serve::Error::Client` with `wire::hub::client::Error::Kind` (`laptop.architect`,
  2026-10-08T15:38:46Z,
  https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6063499704), and a
  body that ends early is `Client` with `Unfinished` (`laptop.architect`,
  2026-10-08T16:52:55Z,
  https://github.com/synnaxlabs/foundation/pull/1918#issuecomment-6064815697).
  `serve::Error::Client` holds each of them (CLIENT HELLO; `laptop.architect`,
  2026-10-11T10:16:56Z,
  https://github.com/synnaxlabs/foundation/issues/2377#issuecomment-6107983027, item
  4). Supersedes `serve::Error::Message` for them, of
  https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6063499704 and
  https://github.com/synnaxlabs/foundation/pull/1918#issuecomment-6064815697.
  Supersedes item 5 (`serve::Error::Hello`) of
  https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6058789738.
  Lost: a `Config::clock` beside `time`, with a loop in `hub` that knows the slew;
  more than one open request, which no wire needs now. Decided by `laptop.architect`
  at 2026-10-08T11:24:07Z
  (https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6058789738).
  `Link` is `Clone`, and a clone is the same link, so the future of each stream holds
  one (NODE PORT) (`laptop.architect`, 2026-10-08T18:56:15Z,
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066908418).
  Supersedes item 6 (`Link` needs no `Clone`) of
  https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6058789738.
  `Hub::set_rules` sets the `access::Rules` that each later hello and request is checked
  against, and `node` calls it with the rules of each spec (#1951). Amended (2026-10-12,
  #1744): `node` calls it on shard 0's hub with the rules of the root region of each
  spec in use, the first before it takes a session. The rules of each followed region,
  and the hub of each other shard, wait on #1951. Approved by `laptop.architect`
  (2026-10-13T05:14:56Z,
  https://github.com/synnaxlabs/foundation/pull/2545#issuecomment-6138768682). Until
  then, a hub has `access::Rules::default()`, which knows no subject, so it refuses each
  hello with `Unknown`. Lost: `hub::Config::rules`, a second way to set one state,
  because the rules change at run time through `Mesh::apply`. Decided by
  `laptop.architect` (2026-10-08T18:05:25Z,
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066050140).
  The name `set_rules`: `laptop.architect`, 2026-10-08T18:56:15Z
  (https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066908418).
  Supersedes the name `Hub::rules` of
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066050140.
  A rule of the client wire, which each SDK follows: a program sends the header of its
  first request or read stream once the challenge after its hello comes. The node sends
  it only after it admits the hello, so this rule also makes the hello stream the first
  that `Link::serve` gets, in any order of the headers. The node does not check the
  rule. A program that breaks it gets `Unadmitted`, `Client` with `Kind`, or a served
  request or read (`laptop.architect`, 2026-10-08T19:22:10Z,
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6067359431).
  Supersedes the result for a program that breaks the rule of
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066239520.
  After the split of CLIENT HELLO, `Client` holds the `Kind` that the comment of
  2026-10-08T19:22:10Z gave as `Message` with `Kind` (`laptop.architect`,
  2026-10-11T11:31:42Z,
  https://github.com/synnaxlabs/foundation/pull/2392#issuecomment-6108567148).
  Supersedes `Message` with `Kind` of
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6067359431.
  Lost: a node that holds each request stream until it admits a hello, which adds a
  queue, its bound, and its timeout to `hub` to save one round trip for each session.
  Decided by `laptop.architect` (2026-10-08T18:16:38Z,
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066239520). The
  sentence on the hello stream is by `laptop.architect` (2026-10-08T18:56:15Z,
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066908418),
  and supersedes the `Session::accept` sentence of that comment. The rule by the
  header of the first request stream is by `laptop.architect` (2026-10-08T19:14:04Z,
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6067215617).
  Supersedes the rule by the send of the first request of
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066239520.
  `node` gives `hub::Config::node` from the key in `node.key` (NODE PORT), as it does
  for the transport and the mesh, and never a zero key. The test is
  `reach_on_a_running_node_dials_its_bound_address` (#1744): a client hello that `node`
  admits through `Hub::link`, with `via` set to the node's key. Decided by
  `laptop.architect` (2026-10-08T18:36:19Z,
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066571400).
  Amended by #1660, where the key moves from `node::Config` to `node.key`
  (https://github.com/synnaxlabs/foundation/issues/1660#issuecomment-6067866831); the
  sentence is by `laptop.architect` (2026-10-08T21:42:22Z,
  https://github.com/synnaxlabs/foundation/pull/1991#issuecomment-6069608203).
  Supersedes the `node::Config::key` clause of
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066571400.
  The request bodies that one hub holds are at most `hub::serve::BODIES_BYTES_MAX`, 2 ×
  `BODY_BYTES_MAX` (32 MiB). A link reserves a body's declared length when it decodes
  the `Request`, before it allocates or reads a byte of the body, and the reservation
  ends when the caller sends or drops its `Reply`. After that, the caller holds the
  body. A body that does not fit stops with `BUSY` (`serve::Error::Bodies`), and the
  link takes its next request. Lost: pool blocks for a body, which take the blocks that
  live writes need; a cap on links as the bound, 16 MiB times the links. Decided by
  `laptop.architect` (2026-10-08T21:34:56Z,
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6069496483); the
  names, and the drop of the node bound, by `laptop.architect` (2026-10-09T03:49:08Z,
  https://github.com/synnaxlabs/foundation/issues/2012#issuecomment-6073876384).
  Supersedes "The bound of a node is the cap times the count of hubs that serve links"
  (https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6069496483).
  The open requests of one subject, over each link of the hub, reserve at most
  `BODY_BYTES_MAX` (16 MiB) at once: its share. The subject is the one of the hello that
  the link admitted, so the hub knows it at decode, before the reservation. A request
  over the share stops with `BUSY` (`serve::Error::Share`) before the hub reads a byte
  of its body, and the link takes its next request. A request over the share and the
  cap gets `Share`. The hub keeps the bytes of each subject, and drops a subject's
  count when it comes to 0. A stalled request still holds its reservation until its
  stream or session ends, but only inside its subject's share. Two subjects together
  can still hold the whole room. Lost: a deadline or a minimum rate for each body,
  because the subject opens its next request at once when the hub stops one, and a
  slow honest link pays the rate too; a stop of stalled bodies when a request does not
  fit, which needs a rate and one task that stops the stream of another link. Decided by
  `laptop.architect` (2026-10-09T12:49:35Z,
  https://github.com/synnaxlabs/foundation/issues/2121#issuecomment-6081223893); the
  name `Share`, and `Share` before `Bodies`, by `laptop.architect`
  (2026-10-09T13:06:31Z,
  https://github.com/synnaxlabs/foundation/issues/2121#issuecomment-6081487506).
  Supersedes "A stalled request holds its reservation until its stream or session
  ends; only an admitted subject can do this"
  (https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6069496483).
  A read takes no request turn: a link serves its reads beside its one open request. The
  transport session's `streams_max` bounds the reads of a link; there is no `READS_MAX`.
  The node holds the open of a read before it checks its signature, so the open
  reserves its length under `BODIES_BYTES_MAX` and the subject's share, as a request
  body does, until it decodes after the wait for the home of its index. An open that
  does not fit stops with `BUSY` (`serve::Error::Share` or `Bodies`). The node opens an
  unnamed `hub::reader::Reader` for the read, only at the home of its index. A read
  whose home is another node stops with `NOT_HOME` (`serve::Error::Reader` with
  `Remote`) before the node opens a stream to that home, and it interns no key set
  (#2419). Decided by `laptop.architect` (2026-10-11T15:17:29Z,
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6110496809, item
  3). Supersedes "runs at the local home or at a remote one" of
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6103306402 item 2.
  `serve::Error::Reader` and `serve::Error::Ended` hold why it did not open or ended,
  and `hub` maps each variant of `reader::Error` and `reader::Ended` to its code in one
  exhaustive `match`: `UNKNOWN` for a selector that matches no channel, `MALFORMED` for
  channels on more than one index, and `FAILED` for the path to the home (`Stream`,
  `Transport`, `Message`, `Frame`, `Credit`). `Unsynced` and `Replaced` cannot come
  for an unnamed reader, so their arms panic. Decided by `laptop.architect`
  (2026-10-10T23:31:47Z,
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6103341608, items 2,
  5, and 6). The ruling approved the plan
  (https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6103306402), which
  gives the other codes (plan item 6): `UNKNOWN` for a removed channel, `BUSY` for the
  pool, `FAILED` for the buffer and the mesh, and the code of the home's refusal for a
  refusal of the home. A complete read that reaches a frame that the buffer lost gets
  `Behind` and the finish of the stream (plan item 3), as a HUB WIRE session does, so
  the arm of `Ended::Behind` panics too (`laptop.architect`, 2026-10-11T15:17:29Z,
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6110496809, item
  1). Supersedes the `Remote` panic arm of
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6103341608 item 6,
  as https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6110496809 item 3
  gives `Remote` the code NOT_HOME. A home's `MALFORMED` gives `FAILED`, as the
  program's open was well formed; `UNKNOWN`, `NOT_HOME`, `BUSY`, and `FAILED` of the
  home keep their code (`laptop.architect`, 2026-10-11T15:17:29Z,
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6110496809, item
  2). Supersedes "the code of the home's refusal" of plan item 6 for `MALFORMED`.
  The program ends a read with the finish of its stream, also while the read waits for
  the home of its index, and the node then finishes the stream. A message after the open
  stops the read with `MALFORMED` (`serve::Error::Client` with `Trailing`)
  (`laptop.architect`, 2026-10-11T17:25:34Z,
  https://github.com/synnaxlabs/foundation/pull/2425#issuecomment-6111681323). A read
  holds the reservation of its open until the home of its index is known and its open
  decodes after that wait. While it waits, it holds its open and the key of its
  index, and no places. After that decode it holds no reservation. A decoded selector
  lives across no wait: the read drops it before the wait for the home, and before it
  sends its first place to the program (`laptop.architect`, 2026-10-11T16:46:33Z,
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6111323686).
  Supersedes "After its open decodes, a read holds no reservation." of
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6110496809 item 4,
  and "until the node decoded it" of
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6103341608 item 5.
  Lost: the decoded selector through the wait, which holds about 26 times the bytes of
  the open with no reservation; and a compact `types::name::Selector`, an interface
  change of `types` that still holds a few times the bytes of the open. It holds its
  places from the last match of its selector, after the wait for the home of its index,
  until the program took its last place. A place is the name, the key, and the sample
  type of one channel that its selector matches. It holds none while it waits for the
  home of its index. For its life it holds the key and the slot of each of those
  channels. So one read holds at most the defined channels of one index, and a link
  holds at most `streams_max` + 1 reads: a client may open one more stream than
  `streams_max`, which `program` and `sdk` keep for a `Class::Command` stream (#2019;
  `laptop.architect`, 2026-10-19T14:03:39Z,
  https://github.com/synnaxlabs/foundation/issues/2019#issuecomment-6269095622). No cap
  bounds the reads of one subject or of one node (#2418). Lost: the places under
  `BODIES_BYTES_MAX` and the share, which refuses a read wider than the share at each
  retry (`laptop.architect`, 2026-10-11T15:17:29Z,
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6110496809, item 4).
  Amended (#2725, with #2723 and #2553): no case gives `Behind` now. A complete session
  that misses a frame reads it from disk, and a seq lost for no room comes as the gap of
  the next frame. The variants stay until #2790 removes them (`laptop.architect`,
  2026-10-16T00:46:53Z:
  https://github.com/synnaxlabs/foundation/issues/2725#issuecomment-6205434126).
  Amended (#2790): the `Behind` variants are removed (HUB WIRE).
