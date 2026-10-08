- **TIME TEXT (#3)** A span is one number and one unit (`ns`, `us`, `ms`, `s`, `m`, `h`,
  `d`). Output uses the largest of `d`, `h`, `m` that divides the span, else the largest
  of `s`, `ms`, `us`, `ns` not more than the span, with a decimal fraction: `3d`, `90s`,
  `1.5s`, `250us`, `0s`. Input takes a decimal fraction and a leading `-` and rejects a
  value that is not a whole number of nanoseconds. A stamp is RFC 3339: output is UTC
  with nine fraction digits; input needs an offset, takes up to nine fraction digits,
  and rejects second 60. A range is the ISO 8601 interval `<start>/<end>`. A `Range`
  never ends before it starts (`Range::new` returns `None`), so its text always round
  trips; input rejects an end before the start. A byte size follows the span rules: one
  number and one unit with no space (`200GiB`), and a decimal fraction only when it
  gives whole bytes (`1.5GiB`). Its type lives in `types` beside `time::Span`, and the
  `document` reader is an adapter over it. The person decided on 2026-10-05 ("A yes I
  approve", #479). The units are `B`, `KiB`, `MiB`, `GiB`, and `TiB`, with exact case:
  `GB` and `Gb` are errors, because they mean other sizes. Input takes no sign. Output
  uses the largest unit that divides the size, with no fraction: `1.5GiB` is written
  `1536MiB`, and zero is `0B` (#505). The reader's `byte::Error` gives the data for a
  fix: where the unit starts, the unit a text likely means (`GiB` for `gib` or `GB`,
  none for `Gb`), and the largest size in the text's unit (#650).
