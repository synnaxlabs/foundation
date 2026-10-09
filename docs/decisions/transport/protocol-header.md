- **PROTOCOL HEADER (#75)** The header of STREAM DISPATCH is 3 bytes: the wire version
  (`u16`, little-endian), then the protocol number (`u8`): clock 1, mesh 2, replica 3,
  blob 4, hub 5. On a stream, the header is the whole first message, so later messages
  carry no prefix. A datagram starts with it; its handler calls
  `Block::skip(wire::header::LEN)` on the rest (BLOCK VIEW). The version covers every
  message on that stream, encoded series included: each wire version fixes one codec
  version (wire 1 carries codec 1). The version comes first and is checked first, so a
  later version can change what follows it. A node reads only `wire::VERSION` until
  version 2 exists; then it also reads the version before it (C9d), and writers take the
  version from the format flag. `node` stops a stream whose header is not valid with
  code 1 (`wire::header::REJECTED`) and resets its reply half, if it has one, with the
  same code; a datagram whose header is not valid drops and counts in a status channel.
  Codes 1 to 15 belong to `wire::header` and `wire::session`, in one space for streams
  and sessions; each protocol numbers its own from 16. `wire::session::REFUSED` (3)
  closes a session that the node does not admit. Lost: a second space for session codes,
  because the hub already closes a client session with the code of its stream, and 1
  would then have two meanings. Decided by `laptop.architect` on #2148
  (2026-10-09T13:37:25Z,
  https://github.com/synnaxlabs/foundation/issues/2148#issuecomment-6082041865).
  Supersedes the sentence of #75 that gave codes 1 to 15 to the header alone, merged in
  https://github.com/synnaxlabs/foundation/pull/90 (2026-10-05), which has no approval
  comment to link. A client session (`Peer::Client`) opens only hub streams, its hello
  stream is the first hub stream, and the node's `Challenge` is the first message on it
  (CLIENT HELLO); `node` refuses the other four protocols from a client. The stream and
  client rules are approved by the coordinator on #90. The hello stream was changed by
  `laptop.architect` at 2026-10-08T09:53:58Z
  (https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6057298526).
  Rejected: a version agreed once per session (the format flag's flip reaches nodes at
  different times, so one session can carry streams of two versions) and a session per
  protocol (`transport` stays blind to protocols, and it costs five handshakes per peer
  pair).
