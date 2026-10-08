- **DOCUMENT KEYS** `document::read::unknown` reports each attribute and each block of a
  body that its reader does not take, with the attribute keys and the block keywords
  apart, so a key that names an attribute never passes as a block. One function holds
  both checks, so a kind cannot forget one half. When a body takes blocks and no
  attribute, as a file does, the fix of an attribute is to move it into one of those
  blocks. `read::missing` reports a body with none of some keys, and panics through
  `one_of` on an empty list, which is a defect of the caller. `read::required` reads one
  key or gives that diagnostic. `config` uses them, also at the top level of a file, and
  so does each kind, so one mistake has one code: `document.unknown-attribute`,
  `document.unknown-block`, and `document.missing-attribute`. Decided by
  `laptop.architect-2` on #1153
  (https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051327019,
  2026-10-08 03:05 UTC) and on #1772
  (https://github.com/synnaxlabs/foundation/pull/1772#issuecomment-6051559819,
  2026-10-08 03:27 UTC, and
  https://github.com/synnaxlabs/foundation/pull/1772#issuecomment-6051578111, 2026-10-08
  03:29 UTC). Lost: a `Body` value that records each key read and reports the rest at
  `finish`, which drops the diagnostics when a caller returns early;
  `unknown_attributes` and `unknown_blocks` as two functions; a public
  `UNKNOWN_ATTRIBUTE` code for a caller to match on. `read::one_of` lists words in
  backticks for a fix, such as "`a`, `b`, or `c`", and panics on an empty list. It is
  public for the `config.bad-action` fix, so no copy goes into `config`. Decided by
  `laptop.architect-2` at 2026-10-08T03:54:12Z
  (https://github.com/synnaxlabs/foundation/pull/1781#issuecomment-6051829474).
  `read::labels::<N>` checks that a block has `N` labels (`document.label-count`),
  `read::repeated` reports each block of a keyword that takes one after the first
  (`document.repeated-block`), and `read::span` refuses a span below zero
  (`document.negative-span`), since each attribute that reads a span needs zero or
  more. The first attribute that takes a negative span adds its own reader, named for
  its meaning, such as an offset. `config` and `connector::reader` use them, and the
  `config.*` codes for these went. Lost: `read::duration` beside `read::span`, since
  A9 names `Span` of any sign a duration; `unknown` with a count for each block, which
  changes each caller of `unknown` for one caller of `repeated`. Decided by
  `laptop.architect-2` at 2026-10-08T07:04:36Z
  (https://github.com/synnaxlabs/foundation/issues/1785#issuecomment-6054474145).
  Supersedes the clause "A negative span reads, and each caller owns its bound" of
  https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6037207886, and its
  fix for `time::Error::Long`, "Use a span from "-106751d" to "106751d"": that fix is
  now "Use a span from "0s" to "106751d"", since each span it names reads. Decided by
  `laptop.architect-2` at 2026-10-08T07:36:01Z
  (https://github.com/synnaxlabs/foundation/pull/1828#issuecomment-6055037900).
