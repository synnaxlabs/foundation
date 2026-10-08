- **LINE TEXT (#1098)** `connector_influx::line::Measurement::new` accepts only text
  that InfluxDB 1, 2, and 3 each store as written, in each part (measurement name, tag
  key, tag value, field key). It refuses the rest at construction, so the error
  reaches the config diagnostic in place of a partial write that InfluxDB answers with
  204 or drops. One set of refused characters holds for every part: a backslash, a
  newline, a carriage return, a tab, NUL, U+FFFD, and each character outside the
  general categories L, M, N, P, and S other than U+0020. The last two are what
  InfluxDB 1 and 2 with `validate-keys` drop (`unicode.IsPrint` false, or
  `unicode.ReplacementChar`). The categories come from `unicode-properties` at Unicode
  17.0.0, which a test pins. InfluxDB reads them from the Unicode tables of the Go
  release that built each server (Unicode 15.0.0 for Go 1.26 today), so a code point
  assigned after that version passes here and that server drops it. No client closes
  this gap exactly. NUL in a tag value is refused, though InfluxDB 3 keeps it. No user
  needs it, and a user learns one rule, not four. Foundation names hold only ASCII
  letters, digits, `_`, `-`, `.`, and `@`, so the rule applies only to text that a user
  writes in the connector's config. Lost: a rule for each part. It keeps NUL in tags
  for no caller, and the set a user may write then depends on the part.
  Decided by the architect (`laptop.architect-2`) on 2026-10-07T06:26:41Z
  (https://github.com/synnaxlabs/foundation/issues/1098#issuecomment-6032314177); the
  class by the architect on 2026-10-07T20:31:16Z
  (https://github.com/synnaxlabs/foundation/issues/1098#issuecomment-6046284871), and
  the crate by the person on 2026-10-08T01:48:32Z
  (https://github.com/synnaxlabs/foundation/issues/1098#issuecomment-6050501586).
