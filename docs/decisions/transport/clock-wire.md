- **CLOCK WIRE (#865)** After the header, a clock datagram is one `wire::clock`
  message: a kind byte, then little-endian fields of 8 bytes. A request (kind 1)
  carries `sent`, the monotonic reading of the node that asks. An answer echoes `sent`,
  so the node that asks keeps no open requests, and carries the peer's time (CLOCK
  PEER ANSWER). Kind 2 is a known bound, with an interval read after the request
  arrived and one read before the answer left. Kind 3 is an unknown bound, with the
  peer's best guess, read after the request arrived and before the answer left. The
  offset of CLOCK PEER ANSWER is this stamp less the asking node's own time, because the
  peer's offset has no meaning without the peer's monotonic clock. A message has 9, 41,
  or 17 bytes, and `decode` refuses each other length. `decode` does not check the order
  of an interval, because `estimate::exchange::Exchange::measure` refuses a crossed one.
  Lost: a request number, because the node that asks must then keep and remove open
  requests and still needs the send time of a late answer; each message 41 bytes, as the
  header has one length (a request then sends 32 zero bytes); `encode` into a
  `&mut [u8]` that returns a length (a short buffer then needs an error); a second byte
  for the kind of time (two checks where one kind byte does the work).
