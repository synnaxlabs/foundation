- **OPCUA SIM (#2515)** `connector_opcua::sim`, behind the cargo feature `sim`, off by
  default, is the simulated OPC UA server of the tests of other crates. It is the
  open62541 test server of the crate, with no security. `sim::Variables::new(nodes)`
  holds variables of the `DataType` `BaseDataType` at the node ids `nodes`, each with
  the Double value 0 and the status Good (`laptop.architect-2`, 2026-10-14T02:30:14Z:
  https://github.com/synnaxlabs/foundation/pull/2635#issuecomment-6159556142). This
  supersedes the Double variables of
  https://github.com/synnaxlabs/foundation/issues/2515#issuecomment-6136027630. Clones
  share them. `Variables::change(quality)` adds 1 to each value and gives each later
  read the status `quality`. `Variables::served()` gives each read that a
  server answered, by the text of its node in `new`, in the order of the answers, as
  `sim::Served { time, value, quality }`, with `time` on the injected clock of the
  server. `sim::serve(clock, wall, net, listener, rng, &variables, until)` serves
  `variables` on the bound `listener` with room for 4 sessions until the
  `cancel::Token` `until` is cancelled or the listener fails, then closes the server,
  and returns the error of the listener, as HTTP SIM SERVER asks of a simulator that a
  conformance scenario stops (`laptop.architect-2`, 2026-10-18T06:35:44Z:
  https://github.com/synnaxlabs/foundation/issues/2641#issuecomment-6246623008).
  This supersedes the `until` that completes of
  https://github.com/synnaxlabs/foundation/pull/2522#issuecomment-6136492404.
  The server clock is the wall time of `wall` plus the span of `Variables::set_clock`
  (`laptop.architect-2`, 2026-10-19T12:37:09Z:
  https://github.com/synnaxlabs/foundation/pull/3354#issuecomment-6267372432). A client
  logs a warning when the time of a channel token is 10 s or more from its own, so the
  caller gives the wall time of the client's node (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/pull/2574#issuecomment-6147681882, 2026-10-13
  14:19 UTC). It adds each namespace from 2 to the highest of `variables`, gives each
  stream of the server the options of `listener`, and panics on a variable of the number
  0, which open62541 would add at a free number. The caller binds the listener, so it
  knows the address before the serve, as with `connector_influx::sim::serve`. The stop
  is an input, so the close drives the streams while the delayed callbacks of open62541
  drain, and each stream sends what it can in that time. What still waits then drops
  with the manager, which resets its stream. On each return, each stream that it
  accepted has ended. A drop of the future closes the server with no wait and leaves
  what waits on each stream unsent. A drop in a panic leaks the server
  (`laptop.architect-2`, 2026-10-18T00:25:33Z and 00:33:08Z:
  https://github.com/synnaxlabs/foundation/pull/3068#issuecomment-6243700840 and
  https://github.com/synnaxlabs/foundation/pull/3068#issuecomment-6243765881). These
  supersede the reason "a drop of the future would leak the server" of
  https://github.com/synnaxlabs/foundation/issues/2515#issuecomment-6136027630. The
  test server of the crate owns the read of each variable: it answers the reads of the
  type check when open62541 adds the node, and frees each read after the delete. Lost:
  the values and the record of reads in C, with a C array that Rust copies out, which
  gives two copies of the state across the FFI, a C allocation per read, and a second
  clock in C. Decided by `laptop.architect-2` (2026-10-13T01:23:17Z:
  https://github.com/synnaxlabs/foundation/issues/2515#issuecomment-6136027630), and the
  one `serve`, with the loss of a `Server` with `listen`, `local`, and `serve`, since a
  drop between `listen` and `serve` leaks the server, the `listener`, `&Variables`, the
  namespaces, and the panic on the number 0 by `laptop.architect-2`
  (2026-10-13T02:02:19Z:
  https://github.com/synnaxlabs/foundation/pull/2522#issuecomment-6136492404), and the
  options of `listener` by `laptop.architect-2` (2026-10-13T02:07:45Z:
  https://github.com/synnaxlabs/foundation/pull/2522#issuecomment-6136612131). The one
  `serve` supersedes the `Server` with `listen`, `local`, and `serve`, and its
  `#[must_use]`, of
  https://github.com/synnaxlabs/foundation/issues/2515#issuecomment-6136027630. The
  `listener` that the caller binds, which `Manager::accepting` takes, supersedes for
  `sim::serve` the bind in `Manager::listening` of
  https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6093207582. Both by
  `laptop.architect-2` (2026-10-13T02:19:56Z:
  https://github.com/synnaxlabs/foundation/pull/2522#issuecomment-6136784257).
  `sim::serve` also serves one fixed tree to browse and import, which the module doc of
  `sim` gives: the folder `ns=1;s=Plant` in the Objects folder, with a folder `Types` of
  one variable of each of 15 built-in types, and a folder `Line` by `HasComponent` with
  a unit, a subtype of Double, of `Enumeration`, and of `Structure`, an array, a
  variable that a client cannot read, and an `Organizes` loop to `Plant`. Each node id
  of a folder or a variable of the tree, other than that of the GUID folder of `Odd`, is
  `ns=1;s=` and the path with `.`. A page of a Browse holds at most 8 references
  (`maxReferencesPerNode`), so `Plant.Types` takes two pages. The tree also adds the
  data types `ns=1;s=Speed`, `ns=1;s=Mode`, and `ns=1;s=Recipe`, and Double
  (`ns=0;i=11`), `Structure` (`ns=0;i=22`), and `Enumeration` (`ns=0;i=29`), which the
  namespace 0 of the build lacks. A variable of `variables` at the node id of a node of
  the tree, these six data types included, panics. No new parameter: each caller gets
  the same tree. Lost: a public builder `sim::Tree` given to `serve`, which repeats the
  test server for callers that need one fixed shape (`laptop.architect-2`,
  2026-10-16T20:14:20Z:
  https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6223855203). The
  folder `Odd` of `Plant` holds a folder of the GUID node id
  `ns=1;g=00000001-0002-0003-0400-000000000000`, a variable by two references, and a
  second variable of its browse name (`laptop.architect-2`, 2026-10-16T21:14:25Z:
  https://github.com/synnaxlabs/foundation/issues/2705#issuecomment-6224815609; approved
  by `laptop.architect-2`, 2026-10-17T00:27:56Z:
  https://github.com/synnaxlabs/foundation/pull/2961#issuecomment-6227331094, at
  7b537d53a, 2026-10-17T00:53:37Z:
  https://github.com/synnaxlabs/foundation/pull/2961#issuecomment-6227618266, and at
  6b83bbdb3, 2026-10-17T01:30:18Z:
  https://github.com/synnaxlabs/foundation/pull/2961#issuecomment-6227973585).
  `Variables::set_clock(ahead)` puts the clock of the server `ahead` of the `wall` time
  of `serve`, in the value of `CurrentTime`, in each server and source timestamp, and
  in the time of each channel token. A later call moves it, as a jump of the clock
  does, and a negative `ahead` puts it behind. Past the last stamp or before the
  first, the clock stays there (`laptop.architect-2`, 2026-10-19T12:37:09Z:
  https://github.com/synnaxlabs/foundation/pull/3354#issuecomment-6267372432). The
  token time shifts too, since open62541 stamps `createdAt` from the same clock, and a
  client only logs a warning when it is 10 s or more off (`laptop.architect-2`,
  2026-10-19T12:02:54Z:
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6266740401). This
  supersedes the token time that does not shift of
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6158936229, and
  the server clock that is only `wall` of item 4 of
  https://github.com/synnaxlabs/foundation/pull/2574#issuecomment-6147681882.
