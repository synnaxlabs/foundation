- **RETENTION (architect, #895)** A retention policy `{ select, keep }` caps by store
  time the holds on the indexes it selects (READER RULES), so `buffer` may trim a sample
  past the cap (STORE TRIM). Retention deletes nothing: a ring frees only at its tail,
  so a time on one index cannot free its samples. It keeps no history window. An index
  that no policy selects has no time cap. `keep` is zero or more. At `0s` the cutoff is
  the mesh time of `home`, from its first estimate (READER RULES). A trim gives a reader
  that is behind a gap at any `keep` (STORE TRIM). Most specific wins as a whole policy
  (X25), a tie between the most specific policies is a plan error (S12, SPECIFICITY),
  and a data channel takes its index's policy (X26). Lost: a finite default `keep`
  (`docs/decisions/open/parameters.md`), a value for "no cap", a size cap per index, and
  a read that reports each sample past `keep` as a gap while its bytes are on disk. That
  read does not depend on disk pressure, but at `0s` a reader a few milliseconds behind
  loses each sample it reads from disk, and each read needs `keep` and a clock. Stale
  commands are the job of `max_age` (A20), not of retention. In `config`, `select` and
  `keep` are both required. `keep` reads with `document::read::span`, which refuses a
  negative span with `document.negative-span` at the `keep` value, as it does a reader
  `hold` (S10, DOCUMENT KEYS; `laptop.architect-2`, 2026-10-08T07:04:36Z,
  https://github.com/synnaxlabs/foundation/issues/1785#issuecomment-6054474145).
  Supersedes the `config.negative-span` code and the clause "A negative span reads, and
  each caller owns its bound" of the ruling at 2026-10-07T11:44:51Z,
  https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6037207886. Ruling
  and answers:
  https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6032219156,
  https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6037207886,
  https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6037251160. The lost
  read: decided by `laptop.architect`, 2026-10-07T12:30:53Z,
  https://github.com/synnaxlabs/foundation/issues/1377#issuecomment-6037946637. The cap
  by store time: decided by `laptop.architect`, 2026-10-07T13:39:39Z,
  https://github.com/synnaxlabs/foundation/issues/1080#issuecomment-6039184732 (READER
  RULES). It supersedes 6037946637 in its clauses "past `keep` after its store time, no
  hold keeps a sample" and "At `0s` no hold keeps a sample after its store time".
  Supersedes https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6032219156
  in its clause that `buffer` trims a sample past `keep`, also when a reader holds it.
