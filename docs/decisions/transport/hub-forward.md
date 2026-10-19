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
  names that it verified. `places::Run` decodes the run one message at a time. It refuses a message with more places than remain, also after the run ended, as `wire::hub::Error::Run`, and a place that is not after the place before it in the run or not below the channels as `wire::hub::Error::Place { place }`. The home gets each as `serve::Error::Message`, which stops the stream with `MALFORMED`. The count needs no bound of its own: a count over the channels fails at the first place that is not below the channels, or the stream ends before the run. The home never reserves `Forward::places` up front. A forward of no place is `Error::Unplaced`. Plan:
  https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6268770318, approved
  with changes by `laptop.architect` at 2026-10-19T14:00:04Z
  (https://github.com/synnaxlabs/foundation/issues/3104#issuecomment-6269021946, change
  1). Kind 8 is the forward of a write. #3356 states how a read is forwarded. The run decoder, kind 8 only, and `Error::Unplaced`: `laptop.architect` at 2026-10-19T22:35:34Z (https://github.com/synnaxlabs/foundation/pull/3444#issuecomment-6278872264). Lost: the keys of the group in place of positions, which the home would check
  against the names that it verified; and a new `serve::Error` variant
  (`serve::Error::Places` exists, for the places of a read selector); and a decode of the whole run, which leaves the join of its messages and their count to the home.
