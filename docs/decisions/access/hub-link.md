- **HUB LINK (2026-10-08)** `Hub::link(session)` gives a `hub::Link` for one transport
  session. `node` calls `Link::serve` for each hub stream that NODE PORT serves once it
  reads its header. On a client link, the first stream given to `serve` is its hello
  stream, and each later one a request stream; `serve` takes the role at the call
  (`laptop.architect`, 2026-10-08T18:56:15Z,
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066908418).
  `hub::Config` gets `node` (the `via` that `admit` checks), `time` (`clock::Reader`),
  and `entropy` (the nonces). The link waits for `Admitted::ends` of a hello with
  `clock::Reader::reach` (CLOCK REACH), so `hub` knows nothing of how mesh time moves
  against the monotonic clock, and gets no second clock. A link has one open request:
  it frees the request when `Reply::send` is called or the `Reply` drops, before the
  first byte of the response, so a client that sends its next request when a reply
  ends never gets `MALFORMED`. `Reply::send` panics on a body over `BODY_BYTES_MAX`, a
  precondition that the maker of the body checks. Each order error names its cause,
  though both stop with `MALFORMED`: `serve::Error::Unadmitted` (a request stream
  before an admitted hello) and `Pending` (a request while one waits for its reply).
  A message of the wrong kind for its stream, such as a hello on a request stream, is
  `serve::Error::Message` with `wire::hub::Error::Kind` (`laptop.architect`,
  2026-10-08T15:38:46Z,
  https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6063499704), and a
  body that ends early is `Message` with `Unfinished` (`laptop.architect`,
  2026-10-08T16:52:55Z,
  https://github.com/synnaxlabs/foundation/pull/1918#issuecomment-6064815697).
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
  against, and `node` calls it with the rules of each spec (#1951). Until then, a hub
  has `access::Rules::default()`, which knows no subject, so it refuses each hello
  with `Unknown`. Lost: `hub::Config::rules`, a second way to set one state, because the
  rules change at run time through `Mesh::apply`. Decided by `laptop.architect`
  (2026-10-08T18:05:25Z,
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066050140).
  The name `set_rules`: `laptop.architect`, 2026-10-08T18:56:15Z
  (https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066908418).
  Supersedes the name `Hub::rules` of
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066050140.
  A rule of the client wire, which each SDK follows: a program sends the header of its
  first request stream once the challenge after its hello comes. The node sends it
  only after it admits the hello, so this rule also makes the hello stream the first
  that `Link::serve` gets, in any order of the headers. The node does not check the
  rule. A program that breaks it gets `Unadmitted`, `Message` with `Kind`, or a served
  request (`laptop.architect`, 2026-10-08T19:22:10Z,
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6067359431).
  Supersedes the result for a program that breaks the rule of
  https://github.com/synnaxlabs/foundation/pull/1946#issuecomment-6066239520.
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
  for the transport and the mesh, and never a zero key. The test waits on #1744, whose
  client hello is the first that a `node` test can admit through `Hub::link`, with
  `via` set to the node's key. Decided by `laptop.architect` (2026-10-08T18:36:19Z,
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
