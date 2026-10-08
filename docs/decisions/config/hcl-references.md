- **HCL REFERENCES (2026-10-05)** The reader reads a reference part by part, as HCL
  reads a traversal: identifiers joined by `.`, with spaces around each `.` and new
  lines inside `[` and `(`. A first part `true`, `false`, or `null` is a value, so
  `true.x` is an index. After a `.`, a number is an index (`site_a.1` is `Form::Index`),
  `*` is a splat, and any other token is a syntax error. An index that is a string with
  no template, quoted or heredoc, is one more segment: `plc["40001"]` is `plc.40001`,
  `a["b"]` is `a.b`, and `plc["a.b"]` is `plc.a.b`. Any other index is `Form::Index`.
  `write` gives each later segment that is not an identifier as a string index
  (`plc["40001"]`, `site_a["@changes"]`). A first segment that does not start with a
  letter or `_`, or that is `true`, `false`, or `null`, has no reference form, and
  `write` refuses it with `Unwritable::Reference`. A file writes such a name as a string
  where a kind takes a name: a kind reads a string or a reference as the same `Name`,
  through `document::read::name` (#474). `read::names` reads one name or a list, in
  order with repeats, and `read::label` reads a block label (architect, #1150,
  [ruling](https://github.com/synnaxlabs/foundation/issues/1150#issuecomment-6037095151)).
  `value::Kind::text` gives the text of a string or of a reference, so each place that
  reads the two as the same text matches them once (`laptop.architect-2`, #1702,
  2026-10-08T06:05:28Z,
  https://github.com/synnaxlabs/foundation/issues/1702#issuecomment-6053513102).
  `export` and `discover` write every name as a string (`"site_a.pt_1"`): they need no
  HCL rule, and a generated file reads back as exactly the Document it came from. This
  replaces the #363 ruling that a file writes a reserved name only as a string. The
  advisor decided (names and architecture delegations, 2026-10-05), #536 and #701. Lost:
  a reserved call `name("40001.x")`, which reserves a function name and adds an error
  for names that a string already carries; it can be added later without breaking a
  file. Lost: bare names in generated files, which changes only how a file looks. Lost:
  `export` and `discover` write only such a name as a string, which copies HCL's
  identifier rule into `config` and layer 3. Lost: `write` gives such a reference as a
  string, which reads back as a `String` and changes the spec hash. Lost: A3 segments
  that start with a letter or `_`, which shrinks the name model to fit one file format.
  The person decided on 2026-10-05 ("a is fine"), #519. Lost: a new `Expected` variant
  for a name after `.`, a public change when the error already names what may come at
  the `.`. #363.
