- **HOME EVERY TYPE (#1145)** `Shard::open_writer` takes a key set with series of any
  `sample::Type`, and the home writes and reads a series of each: `codec` checks and
  encodes it as S3 says, and STORED BODY stores its type. `codec` refuses a `String`
  sample that is not UTF-8 (#556, `laptop.architect`,
  https://github.com/synnaxlabs/foundation/issues/556#issuecomment-6055835549,
  2026-10-08T08:23:59Z). Neither `home` nor `hub` has a
  `writer::Error::Type`. Supersedes HOME TYPE REFUSAL (#963,
  https://github.com/synnaxlabs/foundation/issues/963#issuecomment-6031702785), the
  patch that refused a series of a type other than a scalar until this change. Lost:
  an allow list in `home` that grows one type per PR, because it copies the list that
  `codec` owns; a variant that no path gives, because it misleads each caller that
  matches on it. Decided by `laptop.architect` (2026-10-08T06:36:33Z:
  https://github.com/synnaxlabs/foundation/issues/1145#issuecomment-6053997861). The
  surface was approved by `laptop.architect` (2026-10-08T07:01:09Z:
  https://github.com/synnaxlabs/foundation/pull/1824#issuecomment-6054411394).
