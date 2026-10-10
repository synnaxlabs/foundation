- **BQ5** Each kind owns `async fn run(&self, ctx: Context)`. The supervisor only starts
  and cancels it. `ctx` gives hub sessions, status, run commands, secrets, and cancel.
  `hub` and `home` enforce the rules. `connector` is a library of components plus
  ready-made compositions built only from public parts. Supersedes: r8 Q5 actor with
  device hooks. `kind::Context` gives a run its name, config, cancel, clock,
  randomness, network (`net`), tasks, `writer`, a writer session whose subject is
  the connector's name, `count`, a count of its status (CONNECTOR STATUS), and
  `reader(&reader::Settings)`, a reader session whose subject and name are the
  connector's name (READER SETTINGS). `writer` is as ruled in `laptop.architect-2`,
  2026-10-08T02:21:15Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6050855677.
  `reader` takes the settings that `reader::read` gives and gives the hub's errors as
  they are (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6053530272 and
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6053886044), always
  under the connector's name
  (https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6057225033). These
  supersede the `reader(channels, mode)` of the 02:21:15Z ruling. It is
  not `Send`: a kind's own thread takes clones of the parts it needs
  (`laptop.architect-2`, 2026-10-08T18:07:05Z:
  https://github.com/synnaxlabs/foundation/pull/1944#issuecomment-6066078510).
