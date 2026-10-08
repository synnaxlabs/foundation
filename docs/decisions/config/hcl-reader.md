- **HCL READER (2026-10-04)** `config-hcl` reads HCL with its own lexer and
  recursive-descent parser for the data-only subset (K1, DOCUMENT MODEL), not with
  `hcl-edit`. Evidence on #85: a 2 KB file of 500 nested lists overflowed the stack and
  ended the process, `hcl-primitives` read `-18446744073709551615` as 1, and its errors
  had no fix-it hints. The reader refuses nesting past the Document limit, reads a
  number written with digits only as an exact integer and any other number as a
  float, refuses a float that an `f64` cannot hold (past the largest, or
  rounded to zero from digits that are not all zero), refuses an object key that is a
  number with a fraction, an exponent, or more than 154 digits (HCL can change such a
  key when it makes a string of it), and gives each unsupported HCL form an error with
  a fix-it hint. r3
  section 2 names this fallback. The person chose "Own reader". Supersedes: `hcl-edit`
  in `docs/dependencies.md`.
