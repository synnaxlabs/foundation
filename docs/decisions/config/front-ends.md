- **FRONT ENDS (#337, 2026-10-08)** `ops` takes a table of front ends from `node`, as
  it takes `kinds`, and does not depend on `config-hcl` (K1). `ops::FrontEnd { read:
  fn(Source, &str) -> Result<Document, Vec<Diagnostic>> }` is `Copy` with no
  `#[non_exhaustive]`, so `node` builds it with a struct literal. An error from `read`
  holds at least one problem. The table is a `BTreeMap<&'static str, FrontEnd>`, keyed
  by the extension with no dot (`"hcl"`). The text after the last `.` of a file name
  picks the front end. A file with no front end gives `ops.unknown-extension` with no
  span, as no Document of the file exists (an exception to the span rule of
  DIAGNOSTICS): the message names the path, and the fix names each extension. A
  directory gives each file in it that the table reads, in path order. `Source(i)` is
  the index of the file in the order `ops` reads it, and `ops` keeps the paths to
  print spans. `node` fills the table (#1756) with `config_hcl::read`, its errors
  mapped through `From<&config_hcl::Error> for Diagnostic`. `write` and `update` join
  `FrontEnd` with the first operation that writes a file. Lost: `ops` calls
  `config_hcl::read` (breaks K1), a `FrontEnd` trait (one implementation, no state),
  and `Box<dyn Fn>` (no front end needs state). Decided by `laptop.architect-2`
  (2026-10-08T06:38:41Z,
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6054035444).
