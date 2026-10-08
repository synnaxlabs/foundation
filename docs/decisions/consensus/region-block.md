- **REGION BLOCK (tunable syntax)** `region "site_a" { voters = [...] }` declares a
  region by name prefix. Regions nest like names. Supersedes: K5 voters policy. The
  prefix is a `types::name::Prefix`, which can be empty: the root prefix
  (`Prefix::ROOT`, text `""`) contains each name, so the root region holds each node.
  `mesh` holds it in `region::Founding::prefix` and `region::State`, and checks each
  name against the region with `Prefix::contains`; `ticket::Options.prefix` stays a
  `Name`. Decided by `laptop.architect` (2026-10-07T12:47:19Z):
  https://github.com/synnaxlabs/foundation/issues/1383#issuecomment-6038223777. Each
  field that holds a region's prefix is a `Prefix`: also `ticket::Ticket`'s region (the
  region that the joining node opens with) and the `region` of `Unfit::Outside` and
  `Refused::Outside`, so a ticket for the root region exists. Decided by
  `laptop.architect` (2026-10-07T13:32:35Z):
  https://github.com/synnaxlabs/foundation/issues/1383#issuecomment-6039051758.
