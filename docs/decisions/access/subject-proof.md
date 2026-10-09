- **SUBJECT PROOF (2026-10-08)** `access::Rules::admit` checks a signed
  `types::hello::Hello` and gives an `access::proof::Admitted`, which no other code
  builds. The owner keeps it for the connection, and `Rules::verify` takes it with each
  request. `verify` finds the key in the spec again and checks the time again, so a
  spec that removes the key stops the next request, and no caller writes its own
  expiry check. The signed bytes are a contract for each SDK; each integer is
  little-endian:
  - hello: the 18 bytes `foundation/hello/1`, the subject length (1 byte), the
    subject, the key (32), `via` as a `u128` (16, the reverse of the byte order of its
    UUID text), the connection key (16), the nonce (16), and `expires` in nanoseconds
    (8);
  - request or session open: the 20 bytes `foundation/request/1`, the connection key
    (16), and the exact bytes that the program sent.

  `types::hello::Hello::encode` writes the fields of the hello after the tag, so
  `access` and `wire` share one encoder of the field run, and a test in `types` pins
  its exact bytes (`laptop.architect`, 2026-10-08T10:15:03Z,
  https://github.com/synnaxlabs/foundation/pull/1854#issuecomment-6057658762).

  The tags take the MESH LOG form, so each SDK reads one form, and no tag is a prefix
  of another (`laptop.architect`, 2026-10-08T07:39:12Z,
  https://github.com/synnaxlabs/foundation/issues/1747#issuecomment-6055096691). A
  request is checked with the key of the hello over its connection key, so a request
  signed with another key or for another connection gives `Error::Signature`, and no
  connection check exists. `peer` is the node that carried the hello: this node when the
  program connected to it, else the node whose transport session forwarded it. A hello
  is live while the latest mesh time is before `expires`, and the node holds it live
  until the earlier of `expires` and `proof::CAP` (15 minutes) past the latest mesh time
  at its admission (`Admitted::ends`). Lost: `access` decodes the signed bytes of the
  hello itself (design B), because `access` then owns a decoder of outside input and the
  hello's wire form, which HUB WIRE gives to `wire`; a free
  `verify` of any `&Hello`, which accepts a key that the program picked when a caller
  skips `admit`; and `Error::Connection`, which the signature makes needless. `admit`
  does not check `nonce`: the node that `via` names checks that it is the challenge
  that it sent (#1748; `laptop.architect`, 2026-10-08T08:10:33Z,
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
