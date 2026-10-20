- **SUBJECT PROOF (2026-10-08)** `access::Rules::admit` checks a signed
  `types::hello::Hello` and gives an `access::proof::Admitted`, which no other code
  builds. The owner keeps it for the connection, and `Rules::verify` takes it with each
  request. `verify` finds the key in the spec again and checks the time again, so a
  spec that removes the key stops the next request, and no caller writes its own
  expiry check. The signed bytes are a contract for each SDK; each integer is
  little-endian:
  - hello: the 18 bytes `foundation/hello/1`, the subject length (1 byte), the
    subject, the key (32), `via` (32, the public key of the node), the connection key
    (16), the nonce (16), and `expires` in nanoseconds (8);
  - request: the 20 bytes `foundation/request/1`, the connection key (16), and the
    exact bytes of the body;
  - read open: the 17 bytes `foundation/read/1`, the connection key (16), and the
    exact bytes of the open;
  - write open: the 18 bytes `foundation/write/1`, the connection key (16), and the
    exact bytes of the open (`laptop.architect`, 2026-10-18T02:34:55Z,
    https://github.com/synnaxlabs/foundation/issues/2718#issuecomment-6244779011).

  Amended by #3212: `via` is the public key that the transport session of the program
  proved, in place of the node key. With trust on the first connect, a host in the
  middle that proves its own key could pass the challenge of the node behind it to the
  program, and relay the hello that names that node. That hello names the public key of
  the host in the middle, so the node behind it refuses the hello with `VIA`.
  `Error::Via` gives both public keys, and `hub::Config::public_key` is the `peer` of
  each client session. The tag stays `foundation/hello/1`: no SDK is released, and the
  subject length keeps the old and new bytes apart. Lost: a node key in the challenge,
  which proves nothing; a hello with both keys, two names of one node; a TLS exporter in
  the hello, which needs a new `transport` surface on each carrier. Decided by
  `laptop.architect` (2026-10-19T23:45:44Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279846281).

  A signature binds its kind: `proof::signed(kind, connection, bytes)` with
  `proof::Kind` (`Request`, `Read`, `Write`) gives the signed bytes, and
  `Rules::verify` takes the kind. The kind byte of a message is not in the signed
  bytes, so with one tag a forwarding node could send a signed read open as a request
  body. A later kind adds its own tag. Supersedes "request or session open" and
  `proof::request`. Decided by `laptop.architect` (2026-10-10T23:31:47Z):
  https://github.com/synnaxlabs/foundation/issues/1749#issuecomment-6103341608.

  `types::hello::Hello::encode` writes the fields of the hello after the tag, so
  `access` and `wire` share one encoder of the field run, and a test in `types` pins
  its exact bytes (`laptop.architect`, 2026-10-08T10:15:03Z,
  https://github.com/synnaxlabs/foundation/pull/1854#issuecomment-6057658762).

  The tags take the MESH LOG form, so each SDK reads one form, and no tag is a prefix
  of another (`laptop.architect`, 2026-10-08T07:39:12Z,
  https://github.com/synnaxlabs/foundation/issues/1747#issuecomment-6055096691). A
  request is checked with the key of the hello over its connection key, so a request
  signed with another key or for another connection gives `Error::Signature`, and no
  connection check exists. `peer` is the public key of the node that carried the hello:
  this node when the program connected to it, else the node whose transport session
  forwarded it. A hello
  is live at a node while the latest mesh time is before `Admitted::ends`: the earlier
  of `expires` and `proof::CAP` (15 minutes) past the latest mesh time at its admission.
  Lost: `access` decodes the signed bytes of the hello itself (design B), because
  `access` then owns a decoder of outside input and the hello's wire form, which HUB
  WIRE gives to `wire`; a free `verify` of any `&Hello`, which accepts a key that the
  program picked when a caller skips `admit`; and `Error::Connection`, which the
  signature makes needless. `admit` does not check `nonce`: the node that `via` names
  checks that it is the challenge that it sent (#1748; `laptop.architect`,
  2026-10-08T08:10:33Z,
  https://github.com/synnaxlabs/foundation/pull/1834#issuecomment-6055629911). A
  hosted proof waits on #1832. Decided by `laptop.architect` at
  2026-10-08T07:35:46Z
  (https://github.com/synnaxlabs/foundation/issues/1747#issuecomment-6055033237).
  Changed by #2066: the cap counts from the latest mesh time, not the earliest. With the
  earliest edge, no hello passed both checks once the error passed 2.5 minutes or was
  unknown, so a node with no time source served no program. A hello now lives at most
  `CAP` on the latest edge. When the error shrinks, that edge drops back, and an
  admitted hello lives longer by the drop. At the node that sent its challenge, this
  exposes nothing: the nonce and the connection tie the hello to one session, and each
  request is signed. Triggers: the first request that opens a session which outlives it,
  and the first forwarded hello (BQ12), each bound the life of a hello across a drop.
  The owner of a forwarded hello cannot check the nonce, and its error can differ from
  that of `via`. Lost: the client counts from the earliest edge, which fails at an
  unknown error and ends a session before its renewal past an error of 2.5 minutes; a
  deadline on the node's monotonic clock, which keeps the bound across a drop but adds a
  second timer and a clock input to `hub` for a case that exposes nothing yet. Decided
  by `laptop.architect` at 2026-10-09T01:42:05Z
  (https://github.com/synnaxlabs/foundation/issues/2066#issuecomment-6072526453).
  Supersedes "past the earliest mesh time" in the doc of `CAP` of
  https://github.com/synnaxlabs/foundation/issues/1747#issuecomment-6055033237. Changed
  by #2088: the node clamps the life of a hello at the cap, and refuses none. A renewal
  answers a challenge 5 minutes old, so at a drop of the latest edge of more than 5
  minutes the cap refused each honest renewal. Now the first renewal after a drop ends
  the long life of the old hello. Lost: the end of the session at the drop; a cap from
  the edge of the challenge (two edges); a fresh challenge before each renewal (a wire
  message for the same result). Decided by `laptop.architect` at 2026-10-09T03:15:58Z
  (https://github.com/synnaxlabs/foundation/pull/2088#issuecomment-6073528599).
  Supersedes `Error::Capped` of
  https://github.com/synnaxlabs/foundation/issues/1747#issuecomment-6055033237 and its
  docs of https://github.com/synnaxlabs/foundation/issues/2066#issuecomment-6072526453.
