- **HCL VERDICTS (2026-10-05)** `oracles/conformance/hcl/` holds HCL texts, each with
  the verdict of a pinned HCL version: accepted or refused. For each accepted text, a
  small Go program next to the texts lists the diagnostic code that `read` gives for
  each form outside data in it, such as `hcl.null`. A test checks that `read` accepts
  exactly the accepted texts with no code, refuses each other accepted text only with
  `Error::Form` of the codes listed for it, and refuses each refused text.
  `differences.txt` lists each text where `read` differs from HCL on purpose, with its
  outcome and the decision behind it, and the test checks that outcome instead. For
  each text that reads, `write` must give the bytes of a text in the directory that is
  accepted with no code and is not in `differences.txt`, and those bytes must read as
  the same Document. The program records the HCL
  version. A person runs it by hand when the texts change; CI does not run it and
  needs no Go. It is the only Go code in the repo. The person decided on 2026-10-05
  ("Yeah that's fine", #460); the coordinator approved the plan on #460.
  The program also writes the values HCL reads from each text with only data, in a
  small text form. For each such text that reads and is not in `differences.txt`, the
  test prints the Document in the same form, and the two must be equal. So the test
  checks which numbers are integers (HCL READER), and it compares the bits of each
  float with the `f64` nearest to the written number. HCL holds a 512-bit value,
  and a second rounding to `f64` can miss the nearest one. Lost: cty JSON, which has
  no value for a reference, a call, or a block, and gives a number as a 512-bit
  decimal; the shortest decimal of a float, which Go and Rust write differently for
  some floats; Rust that reads the form into a Document, which is more code than a
  printer; and Go that writes `document::encoding`, a second implementation of the
  encoding. Decided by the `config` builder (#497).
