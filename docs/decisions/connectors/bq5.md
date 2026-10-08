- **BQ5** Each kind owns `async fn run(&self, ctx: Context)`. The supervisor only starts
  and cancels it. `ctx` gives hub sessions, status, run commands, secrets, and cancel.
  `hub` and `home` enforce the rules. `connector` is a library of components plus
  ready-made compositions built only from public parts. Supersedes: r8 Q5 actor with
  device hooks. `kind::Context` gives a run its name, config, cancel, clock,
  randomness, network (`net`), and tasks, and `writer`, `reader`, and `status` come
  with #1731 (`laptop.architect-2`, 2026-10-08T02:21:15Z:
  https://github.com/synnaxlabs/foundation/issues/1731#issuecomment-6050855677). It is
  not `Send`: a kind's own thread takes clones of the parts it needs
  (`laptop.architect-2`, 2026-10-08T18:07:05Z:
  https://github.com/synnaxlabs/foundation/pull/1944#issuecomment-6066078510).
