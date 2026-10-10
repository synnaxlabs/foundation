- **DIAGNOSTICS (2026-10-05)** A problem that a person or an agent fixes in a
  Document or its file is a `document::diagnostic::Diagnostic`: a stable `Code`, a
  span, a message, a fix, and notes (other places that explain it). The span is `None`
  only for a Document with no spans; a problem with a whole file has an empty span at
  the start of the file. The message and the fix have no final period, and each
  producer's tests pin both. A code is `<producer>.<problem>`: each part is lower-case
  ASCII letters and digits, starts with a letter, and may join words with single `-`
  (`hcl.syntax`, `document.duplicate-key`). The producer is a name it owns: the syntax
  of a front end, a core crate, or a kind. No two producers share a name. A producer
  declares each code as a `const` item, so a bad code fails the build. A code never
  changes between releases. Each producer maps its own errors with `From<&Error>`
  beside them, so `config`, `ops`, and `node` never match a producer's variants. An
  error from a crate below `document` that a producer shows as a diagnostic gives its
  message with `Display` and its fix with `fix()`; the producer adds the code and the
  span. A fix that shows a value in a Document shows it as the file writes it, so
  `document.bad-size` quotes the size for `Syntax` and `Range` (`Use at most
  "16777215TiB"`), while `byte::Error::fix` stays bare for a flag (architect,
  https://github.com/synnaxlabs/foundation/issues/1070#issuecomment-6032077046).
  `size_fix` stays in `document` (#650): it works on the text the reader read, and its
  only caller is the reader. It moves to `types` when a second reader of sizes needs it
  (same ruling).
  `Diagnostic` is `#[non_exhaustive]`, so a new field with a default in `new`
  breaks no producer. No severity field: the warnings in K2 and R13-10 belong to plan
  output.
  `ops` operation error codes use `Code` too, so the grammar has one home. A code
  crosses the wire as text, and no reader makes a `Code` from it. Lost: a `Diagnose`
  trait behind `Box<dyn>` (not `Clone`, and a fix is optional); number codes (a
  central registry, and unreadable); one span only (the first producer has two
  places). Codes go into `oracles/conformance/document/` at the first stable release;
  the person decided on 2026-10-05 ("At the first release"). Decided by the `config`
  builder; approved by the coordinator (#137).
  A message or a fix quotes text from a file with `types::text::Quoted`: U+0020 to
  U+007E as written, except `\"`, `\\`, and `$` or `%` for a `$` or `%`
  before `{`; each other character as `\u` and four upper-case hex digits, or `\U` and
  eight above U+FFFF. HCL, YAML, and TOML read the form back as the text, and a
  look-alike shows. The `config-hcl` writer keeps its own rule, because a person edits
  what it writes. Lost: Rust's `Debug` form, which no file reads; `$$` and `%%`, which
  only HCL reads. Decided by the architect (#941).
  Until `types::text::Quoted` is on `main` (#941), a producer quotes text from a file
  with `{:?}`, and #941 changes each such quote to `Quoted`. Decided by the architect
  at 2026-10-08T05:49:29Z
  (https://github.com/synnaxlabs/foundation/issues/941#issuecomment-6053293087).
  `document::Position` and `document::Span` derive `PartialOrd` and `Ord`: a position
  by offset, then by line and column, and a span by source, then by start, then by
  end. This agrees with `Eq`. `config` sorts its diagnostics and labels by span, no
  span first, and `ops` sorts its plan lines by span, so the rule has one home. Two
  diagnostics at one start order by their end. Lost: a public `Span::order`, a key
  that each caller must know, which does not agree with `Eq` (`laptop.architect-2`,
  2026-10-10T02:45:50Z,
  https://github.com/synnaxlabs/foundation/issues/1914#issuecomment-6092943513).
