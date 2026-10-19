- **BQ5** Each kind owns `async fn run(&self, ctx: Context)`. The supervisor only starts
  and cancels it. `ctx` gives hub sessions, status, run commands, secrets, and cancel.
  `hub` and `home` enforce the rules. `connector` is a library of components plus
  ready-made compositions built only from public parts. Supersedes: r8 Q5 actor with
  device hooks. `kind::Context` gives a run its name, config, cancel, clock,
  randomness, network (`net`), tasks, `writer`, a writer session whose subject is
  the connector's name, `count`, a count of its status (CONNECTOR STATUS;
  `laptop.architect-2`, 2026-10-09T19:43:41Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6088037219, item
  3), and `reader()`, which takes the reader session that the supervisor opened for the
  run, whose subject and name are the connector's name (READER SETTINGS, SUPERVISOR).
  `writer` is as ruled in `laptop.architect-2`, 2026-10-08T02:21:15Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6050855677, and it
  gives `Ok(None)` once the run's cancel is cancelled, also before the call, so a wait
  for the home of an index ends at the cancel (`laptop.architect-2`,
  2026-10-10T16:10:07Z:
  https://github.com/synnaxlabs/foundation/pull/2150#issuecomment-6099522825). Lost:
  each kind races the cancel itself, and a `Cancelled` error. The supervisor's open of
  the reader for a run also ends at the cancel, and then the run does not start
  (SUPERVISOR). Supersedes the `Result<hub::writer::Writer, hub::writer::Error>` of
  `writer` of
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6050855677. The
  supervisor opens the reader from the settings that `reader::read` gives
  (`laptop.architect-2`, 2026-10-08T06:06:40Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6053530272), always
  under the connector's name (2026-10-08T09:49:37Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6057225033).
  `writer` and the open of the reader give `kind::Error`, and `connector` chooses the
  class of each hub error in one place (KIND TABLE), since the class depends on the hub
  error and not on the kind. Lost: the hub's errors as they are, which each kind maps
  (`laptop.architect-2`, 2026-10-10T22:38:41Z:
  https://github.com/synnaxlabs/foundation/issues/1734#issuecomment-6102937379).
  Supersedes the hub's errors as they are of
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6053886044.
  Supersedes the `reader(channels, mode)` and the `status()` of
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6050855677.
  Supersedes, as 6057225033 rules, the default name for a `None` `settings.name` of
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6053530272 and
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6053886044.
  Run commands wait for #2300. `secret(&Name)` reads a secret from the store of the
  supervisor (SECRET STORES AS ADAPTERS). It is not `Send`: a kind's own thread takes
  clones of the parts it needs
  (`laptop.architect-2`, 2026-10-08T18:07:05Z:
  https://github.com/synnaxlabs/foundation/pull/1944#issuecomment-6066078510).
