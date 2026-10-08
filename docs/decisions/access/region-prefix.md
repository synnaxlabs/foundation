- **REGION PREFIX** `access::Rules::new` takes the definitions of each region tree,
  with the region as a `types::name::Prefix`; `Prefix::ROOT` is the root region. Access
  picks out the policies, connectors, and subjects itself. A policy reaches a name when
  `Prefix::contains` holds, so no caller writes the root case. Decided by
  `laptop.architect` on 2026-10-07T12:47:19Z
  ([#1383](https://github.com/synnaxlabs/foundation/issues/1383#issuecomment-6038223777));
  applied in #1402. The trees in place of the policies: `laptop.architect`,
  2026-10-08T03:01:36Z
  ([#810](https://github.com/synnaxlabs/foundation/issues/810#issuecomment-6051285927)).
  The subjects: `laptop.architect`, 2026-10-08T06:56:19Z
  (https://github.com/synnaxlabs/foundation/issues/1747#issuecomment-6054321636), by
  their tree key at 2026-10-08T08:10:33Z
  (https://github.com/synnaxlabs/foundation/pull/1834#issuecomment-6055629911), and by
  their label at 2026-10-08T11:00:08Z
  (https://github.com/synnaxlabs/foundation/pull/1862#issuecomment-6058397812), which
  supersedes ruling 1 (the tree key) of
  https://github.com/synnaxlabs/foundation/pull/1834#issuecomment-6055629911 (SUBJECT
  KEYS).
