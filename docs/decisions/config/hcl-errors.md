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
