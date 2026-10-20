- **HUB FORWARD (#3104, 2026-10-19)** A node carries a program's write to the home of
  its index on one node-to-node hub stream, a forward. After the header, the node sends
  `wire::hub::forward::Forward`: kind 8, then the count of places (`u32`), at least 1.
  Then the program's signed hello (kind 4) and its signed first message (`Write`, kind
  7) as the program sent them, then the open body, the bytes that the program signed.
  Then the run of `forward::places`: exactly that count of places, each a position
  (`u32`) in the channels of the open, after the place before it and below the channels.
  Kinds 1 to 3 are of HUB WIRE and 4 to 7 of HUB CLIENT, so the home tells a forward
  from a reader `Open` by its first byte. Positions, not keys, name the group, so the
  home opens only names that it verified. `places::Run` decodes the run one message at a
  time. It refuses a message with more places than remain, also after the run ended, as
  `wire::hub::Error::Run`, and a place that is not after the place before it in the run
  or not below the channels as `wire::hub::Error::Place { place }`. The home gets each
  as `serve::Error::Message`, which stops the stream with `MALFORMED`. The count needs
  no bound of its own: a count over the channels fails at the first place that is not
  below the channels, or the stream ends before the run. The home never reserves
  `Forward::places` up front. A forward of no place is `Error::Unplaced`. Plan:
  https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6268770318, approved
  with changes by `laptop.architect` at 2026-10-19T14:00:04Z
  (https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6269021946, change
  1), and the name `Error::Place` by `laptop.architect` at 2026-10-19T18:59:22Z
  (https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6274918681). Kind 8
  is the forward of a write. #3356 states how a read is forwarded. The run decoder and
  no bound of the count, kind 8 only, and `Error::Unplaced`: `laptop.architect` at
  2026-10-19T22:35:34Z
  (https://github.com/synnaxlabs/foundation/pull/3444#issuecomment-6278872264). Lost:
  the keys of the group in place of positions, which the home would check against the
  names that it verified; and a new `serve::Error` variant (`serve::Error::Places`
  exists, for the places of a read selector); and a decode of the whole run, which
  leaves the join of its messages and their count to the home; and a check of
  `Forward::places` against the channels in `Run::new`, an error path of its own for a
  case that the place check or the end of the stream ends (#3104 PR 3b, from
  https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6279360925, item
  2).

  At the home (#3104 PR 3b), `Link::serve` takes the member of the hub's region that
  holds the node's key at the call, and `serve::run` tells a forward from a reader
  `Open` by `Forward::decode`. A forward from a node that is not a member panics: the
  route checks the member in the same turn. The home admits the hello with the public
  key that the node's transport session proved as its carrier (SUBJECT PROOF), and with
  no nonce check, as `access::proof` states. From then on, the stream races the end of
  the hello, which stops it with `EXPIRED`. The stream polls first, so a renewal moves
  the end in the poll that admits it. The home reads the `Write` and its body as a
  program's write, verifies the open, holds its decoded names and 4 bytes for each
  channel of the open (MEMORY BOUNDS), and takes the places. The end of the stream
  before the last place finishes the stream and opens nothing. The group is the channels
  of the open at the places, and the write goes on as a program's write, with the member
  as the `via` of its handoff. A group on more than one index, before the wait for the
  home or after a change of a channel during it, stops the forward with
  `serve::Error::ManyIndexes` (`MALFORMED`), and no control is taken
  (`laptop.architect`, 2026-10-20T06:52:11Z:
  https://github.com/synnaxlabs/foundation/pull/3384#issuecomment-6285196419). A message
  of kind 4 is a renewal only during the wait for the home and between frames, where
  `write::Frames::between` is true
  (`laptop.architect`, 2026-10-20T01:49:00Z:
  https://github.com/synnaxlabs/foundation/pull/3480#issuecomment-6281429498). A message
  inside a frame has no kind byte, so a first byte of 4 there is data. A refusal of the
  proof stops the stream with its code. PR 4 gives `REFUSED`, `VIA`, and `CHANGED` to
  the program as `wire::hub::write::End::Refused` (code 3), and `EXPIRED` as
  `End::Failed`. Plan:
  https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6280544339 with
  https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6280551038.
  `End::Refused`: `laptop.architect` at 2026-10-19T14:00:04Z
  (https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6269021946, answer
  3). The surface, the text of the panic, and the renewal only between frames:
  `laptop.architect` at 2026-10-20T00:41:09Z
  (https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6280579045). PR 4
  states the renewal rule as a rule of the forwarding node. Lost: a forward as a client
  session from the node to the home for each program connection, which needs no
  dispatch, but the home would check a nonce that the program never saw.

  At the member (#3104 PR 4a), a program's write whose one index has a home on another
  node of the region goes to `serve::client::write::away`. A write on more than one
  index with a home on another node still stops with `NOT_HOME` until PR 4b
  (`laptop.monitor`, key choice 1, 2026-10-20T06:26Z:
  https://github.com/synnaxlabs/foundation/pull/3546#issuecomment-6286607969). It
  reserves the ends and counts of the open and the encoded open, `Write::length`, in the
  subject's share, and encodes the open again from its decoded fields. Each field of the
  wire `Open` has one encoding, so these are the bytes that the program signed, and the
  member holds the names once while it waits for the home. Lost: keeping the program's
  body, which holds the names twice during that wait. `hub::dial` dials the home and
  retries a session that closes with `Code(0)` before the places, as the remote reader
  does. A forward to a home whose stream takes messages under the member transport's
  `message_bytes_max` gives the program `FAILED` at the open, before the places, so a
  mesh whose limits differ fails at its first forwarded write. The member gives
  `serve::Error::Limit`, whose text names both limits and tells the user to give each
  node of the mesh the same limit (condition of `laptop.monitor` on key choice 3,
  2026-10-20T06:33Z:
  https://github.com/synnaxlabs/foundation/pull/3546#issuecomment-6286607969).
  A reply before the places that `Opening` refuses, or a count of places over the
  channels of the open plus one, gives `FAILED` to the program and `MALFORMED` to the
  home. The member holds the places in the subject's share until it sends them to the
  program, so a home that sends more than the share holds gives `BUSY`. While it waits
  for the places, it sends the home each renewal of the hello. A code of the home before
  the places stops the program's stream with that code, `NOT_HOME` included, and with
  `FAILED` in place of `MALFORMED`, as `serve::Error::Forward { home, refusal }`, and an
  end of the stream with no code gives `FAILED`. The member finds no new home (#3541). A
  write whose lease is zero gets `MALFORMED` before the find of its home, not the
  `FAILED` of a forward that the home refuses. After the places, the member splices:
  each message of the program goes to the home as it came, each renewal of the hello
  goes between frames, and each reply goes to the program as it came. It reads no more
  of the program while 64 frames have an outcome due, so a forward moves at most 64
  frames in each round trip to the home (answer 4 of
  https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6269021946). One
  task counts the replies of a forward before it reads the program, in each poll, so
  the window keeps no waker. A join of many homes keeps that order (#3565;
  `laptop.architect`, 2026-10-20T17:23:34Z:
  https://github.com/synnaxlabs/foundation/pull/3628#issuecomment-6296895024, item 3).
  When the forward is cut, the member resets the program's write with the code of the
  home, and with `FAILED` in place of `MALFORMED`.
  It also gives `FAILED` for a reply that
  `Replies` refuses, a finish of the home before the program's, a finish with an outcome
  due, and an error of the session to the home. For the first three, it stops the home
  with `MALFORMED`, which the home gets only while it does not have each message of
  the member. A reply that the member read but did not send has an unknown fate,
  as for each reset of a write. Lost: an end of group 0 with `Reply::Ended`, which needs
  an end of unknown fate in `wire` (PR 4b). The cut reset: `laptop.monitor`, key choice
  4, 2026-10-20T08:17:05Z
  (https://github.com/synnaxlabs/foundation/pull/3546#issuecomment-6286607969), and
  `laptop.architect`, 2026-10-20T08:21:06Z, with the lease check as its condition
  (https://github.com/synnaxlabs/foundation/pull/3546#issuecomment-6286674415). A close
  of the program's session resets the stream to the home with code 0. A split of each
  message for a home with smaller messages: #3542, not before the demo. Plan:
  https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6284841084, with the
  changes of
  https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6284933117. The doc
  of `Frames::skip` and `serve::Error::Forward`: `laptop.architect`,
  2026-10-20T06:53:02Z
  (https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6285208337).
  `serve::Error::Limit`, with the stop at the open: `laptop.architect`,
  2026-10-20T07:12:26Z
  (https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6285493756).

  The cost of a forward is the member's serve: in the `forward` bench of `hub` (#3484),
  the home's serve of a forward is within 3% of a direct write at the median of 6 runs
  on the quiet host:
  https://github.com/synnaxlabs/foundation/pull/3579#issuecomment-6290402550. It is over
  the cost limit of P1 (`docs/claude/performance.md`), and accepted: at 1x64 the member
  costs 35 ns a sample against 68 ns for the home's write on the quiet host, so a
  forward never sets the ceiling of a write, and a program that writes at its home pays
  nothing new (`laptop.architect`, 2026-10-20T11:39:00Z:
  https://github.com/synnaxlabs/foundation/pull/3579#issuecomment-6289997149). Look
  again when a measured workload shows the member's forward, not a home's write, limits
  the rate of a node. The same ruling limits the member line to 2.5 µs on the quiet
  host. A session with 62 other forwards takes 2.57 µs there, so #3583 brings it under
  before PR 4b. These numbers are of the hub's serve on the `sim` driver. #3584
  measures the carrier work that a forward adds at the member, and the `os` driver.
  When the member's serve and its carrier work a sample are at or over the home's write
  and its carrier work a sample at 1x64, this judgment comes back to `laptop.architect`
  (2026-10-20T11:52:51Z:
  https://github.com/synnaxlabs/foundation/pull/3579#issuecomment-6290226466).
