- **CLIENT HELLO (2026-10-08)** A program's session with the node it connects to
  has a hello stream and request streams (`wire::hub::client`, kinds 4 and 5). Hub
  kinds are one space: the first byte of a hub stream picks its decoder
  (`laptop.architect`, 2026-10-08T10:19:16Z,
  https://github.com/synnaxlabs/foundation/pull/1854#issuecomment-6057726181). The
  hello stream is the first hub stream and lives as long as the session. The node sends
  a `Challenge` (a fresh nonce and its mesh time) first and after each admitted hello;
  the program sends a `Signed` hello that echoes it, first and to renew at half its
  life. The wire hello is kind 4, the signed bytes of the hello with no tag
  (`Hello::encode`), and the signature, so an SDK writes one form (`laptop.architect`,
  2026-10-08T10:15:03Z,
  https://github.com/synnaxlabs/foundation/pull/1854#issuecomment-6057658762). Each
  refusal on the hello stream ends the session. A request stream carries one
  `Request` and its body, then one `Response` and its body; each body is a run of at
  most `BODY_BYTES_MAX` (16 MiB), which the SDK checks before it sends. A body over the
  cap at the node gets `MALFORMED`. `access::Rules::renew` checks a renewal as `admit`
  checks a first hello, after `Error::Changed` for another subject, key, `via`, or
  connection, and takes the carrier from the `Admitted`: a move to another node is a
  new connection. `Changed` names the first field that differs, an
  `access::proof::Field` (`laptop.architect`, 2026-10-08T10:19:16Z,
  https://github.com/synnaxlabs/foundation/pull/1854#issuecomment-6057726181).
  Stop codes: `REFUSED` 20 for an unknown subject, an unlisted key, or
  a bad signature, which tell about the spec and so share one code (`Signature` too:
  `admit` checks it only for a listed key); each other step its own code, `UNSYNCED`
  21, `STALE` 22, `VIA` 23, `EXPIRED` 24, `CAPPED` 25, `CHANGED` 26. `hub` maps each
  `access::proof::Error` to its code in one exhaustive `match`, and `serve` returns the
  exact error for the node's log. `connection::Key` writes as UUID text in byte order.
  Lost: one `REFUSED` for each refusal, which hides an unsynced node from a program;
  and a verify before the spec lookup, so that each refusal costs the same, which costs
  a verify for each hello from an unknown client. Decided by `laptop.architect` at
  2026-10-08T09:53:58Z
  (https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6057298526),
  which extends the refusal ruling of #1744
  (https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6053329389).
  Each side knows the kind of each stream, so it calls the `decode` of the message that
  it expects (`Challenge`, `Signed`, `Request`, `Response`), each of which refuses
  another kind with `Error::Kind`; a request or response gives its `Body`, which counts
  the body's messages (`laptop.architect`, 2026-10-08T15:38:46Z,
  https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6063499704). When
  the stream ends, `Body::end` gives `Error::Unfinished` if bytes of the body remain, so
  each rule of a body is in `wire` (`laptop.architect`, 2026-10-08T16:52:55Z,
  https://github.com/synnaxlabs/foundation/pull/1918#issuecomment-6064815697). Lost:
  a decoder for each side that takes the kind of the stream from its first message,
  because each caller checks the kind again; `Gateway::hello()` and
  `Gateway::request()`, the kind at construction, because each caller still matches
  variants that its stream cannot carry; and `Gateway` and `Program` for request
  streams only, a header-or-body enum where the caller knows which comes.
  Supersedes shape decision 1 of #1854
  (https://github.com/synnaxlabs/foundation/pull/1854#issuecomment-6057726181), one
  decoder for each side that takes the kind of the stream from its first message.
