- **HCL IDENTIFIERS (2026-10-05)** The reader accepts identifiers outside ASCII as HCL
  does (Unicode `XID_Start` and `XID_Continue`, through `unicode-ident`), so
  `température = 1` reads. A new error for each such identifier lost: a valid HCL file
  would fail. The person decided on 2026-10-05 ("go with yes"), with low priority, #263.
  A reference outside ASCII is still `hcl.name`, because names are ASCII (A3).
  Measured against HCL v2.25.0, two differences remain. HCL reads the 23 compatibility
  characters in `ID_Start` but not in `XID_Start` (U+037A, U+0E33, and others). The
  reader refuses them at the start of an identifier, and 19 of them after it. The
  reader follows the Unicode version of `unicode-ident` in `Cargo.lock`, which can be
  newer than HCL's, so it accepts characters that HCL does not know yet. Lost: a
  hand-kept list of the 23; own tables generated from HCL's Unicode version; and
  `unicode-id-start`, a second table crate that follows the changes JavaScript makes to
  `ID_Start` and `ID_Continue`.
