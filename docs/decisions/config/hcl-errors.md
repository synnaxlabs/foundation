- **HCL ERRORS (2026-10-05)** Each function of `config-hcl` gives only the errors it
  can have. `read` gives a list of `Error`, `write` a list of `Unwritable`, and
  `update` a `Refusal`: the problems in the old text, or else the parts of the new
  Document that HCL text cannot hold. Nesting past the depth limit is
  `Error::TooDeep` from `read`, with `document`'s diagnostic; `write` and `update`
  take a `Checked` Document, so they cannot meet it. Lost: one `Error` for all three,
  so each caller of `read` handled a variant that `read` never gives; a `write` that
  takes a plain Document and clones it into a `Checked`, which copies each tree only
  to check its depth and keeps `Unwritable::TooDeep`; and an `update` that takes the
  Document that `read` gave for the text, so it gives only `Unwritable`, but writes
  wrong text with no error when a caller gives another Document. Decided by the
  `config` builder; approved by the coordinator (#330). `write` and `update` take a
  `Checked`, and `read` does not change: decided by the architect (#828,
  https://github.com/synnaxlabs/foundation/issues/828#issuecomment-6030891911).
  Trigger for `read` to give a `Checked`: the first production code that calls
  `Checked::new` on a `read` result, or writes one back with `write`, files an
  `interface` issue that names it. Until then no code checks the depth twice, and a
  checked whole Document does not make a checked connector body. Decided by the
  architect (#1089,
  https://github.com/synnaxlabs/foundation/pull/1089#issuecomment-6031438972).
- **HCL DIAGNOSTICS (#2079, 2026-10-10)** `config_hcl::read` gives `Result<Document,
  Vec<Diagnostic>>`, the shape of `ops::FrontEnd::read`: a diagnostic for each problem
  in the text, at least one, in source order. `Error`, `Expected`, `Unclosed`, `Form`,
  `Number`, and `From<&Error> for Diagnostic` are private, and `Refusal::Text` holds
  `Vec<Diagnostic>`. Each front end gives the neutral model at the boundary, and no
  caller read an `Error` variant. `node` and the `ops` tests use `FrontEnd { read:
  config_hcl::read }`, with no adapter, and `node` no longer takes `document`, which
  only the adapter used. When `write` and `update` join `FrontEnd` (FRONT ENDS), they
  give `Vec<Diagnostic>` by the same rule, and `Refusal` and `Unwritable` go private.
  The crate's tests keep exact `Error` values through the private type. Lost: a second
  public `read` that gives diagnostics next to the one that gives `Error` (two reads
  that differ only in the error type). Supersedes "`read` gives a list of `Error`" and
  "Nesting past the depth limit is `Error::TooDeep` from `read`" of HCL ERRORS, and
  "`node` also takes `document`" of NODE MESH. Decided by `laptop.architect-2`
  (2026-10-10T02:51:42Z,
  https://github.com/synnaxlabs/foundation/issues/2079#issuecomment-6093002544);
  `Number` and the `# Errors` text of `read` approved by `laptop.architect-2`
  (2026-10-10T03:09:27Z,
  https://github.com/synnaxlabs/foundation/pull/2222#issuecomment-6093159318); the
  oracle edit approved by the person (2026-10-10T02:58:30Z,
  https://github.com/synnaxlabs/foundation/issues/2079#issuecomment-6093074742).
