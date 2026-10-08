- **DIAL ORDER (#68, 2026-10-07)** `Transport::dial` tries a peer's addresses UDP, then
  TCP, then relays, in the given order within a kind. It starts the next address 250 ms
  after the newest attempt started, or at once when the newest fails, and keeps the
  first session that completes; dropping the others closes them. Before it starts a
  carrier, it checks each address: port 0, an unspecified IP, or a kind with no carrier
  on this node is the cause `Error::Unroutable` in `Error::Unreachable`, so
  `Endpoint::connect` keeps its invariant panic. A broken socket ends the dial with
  `Error::Network`. Rejected: the carrier maps noq-proto's invalid address to an error
  (each later carrier would need its own check), and `Network` with neutral text (one
  variant with two meanings: a caller cannot tell a dead socket from a bad address). An
  attempt that connected before the break still wins, and its session ends with
  `Error::Network`, as `accept` gives such a session, so the result does not depend on
  the order of the attempts. The order and the stagger: proposed by `box2.builder-5`
  (https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6022920297), decided
  by the architect in review of #1067. `Unroutable`: decided by the architect
  (https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6030703879). The
  connected attempt: proposed by `box2.builder-5`, decided by the architect
  (https://github.com/synnaxlabs/foundation/issues/68#issuecomment-6030913321).
