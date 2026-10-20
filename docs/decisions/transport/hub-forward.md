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
