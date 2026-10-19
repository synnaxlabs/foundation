- **HUB FORWARD (#3104, 2026-10-19)** A node carries a program's write to the
  home of its index on one node-to-node hub stream, a forward. After the header, the
  node sends `wire::hub::forward::Forward`: kind 8, then the count
  of places (`u32`), at least 1. Then the program's signed hello (kind 4) and its signed
  first message (`Write`, kind 7) as the program sent them, then the
  open body, the bytes that the program signed. Then the run of
  `forward::places`: exactly that count of places, each a position (`u32`) in
  the channels of the open, after the place before it and below the channels. Kinds 1 to
  3 are of HUB WIRE and 4 to 7 of HUB CLIENT, so the home tells a forward from a reader
  `Open` by its first byte. Positions, not keys, name the group, so the home opens only
  names that it verified. `places::decode(run, channels)` refuses a place that is not
  after the place before it or not below `channels` as `wire::hub::Error::Place { place
  }`, and the home gets it as `serve::Error::Message`, which stops the stream with
  `MALFORMED`. A forward of no place is `Error::Unplaced`. Plan:
  https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6268770318, approved
  with changes by `laptop.architect` at 2026-10-19T14:00:04Z
  (https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6269021946, change
  1). Kind 8 is the forward of a write. #3356 states how a read is forwarded. Lost: the keys of the group in place of positions, which the home would check
  against the names that it verified; and a new `serve::Error` variant
  (`serve::Error::Places` exists, for the places of a read selector).
