- **OPS OUTPUT (#337, 2026-10-08)** The plan output is a typed `plan::Output { base,
  changes, homes, added, changed, removed }`, with `Change { action, kind, name, place:
  Option<Place> }`. It derives `Serialize`, `Deserialize`, and `JsonSchema`, and `ops`
  resolves each span to a file, line, and column. `text()` gives the terminal text, as
  `Response` and `Error` do. #1744 makes it a `Response` variant. Each error of `ops`,
  in the CLI and in MCP, has one JSON shape: `{"errors": [...]}`, each item a typed
  `Problem { code, message, fix, place: Option<Place>, notes }`. An error that is not a
  diagnostic, such as `ops.argument`, is a list of one item with no place. A place is
  `{"file", "line", "column"}`, each count from 1, and the key is absent when there is
  no place. `notes` is a list, empty when there is no note. The text of an error is each
  item as `error[<code>]: <message>`, its place, `fix: <fix>`, and each note, with an
  empty line between two items. Lost: a second shape for diagnostics, with which a
  client parses two shapes and must know which operation gives which. Decided by
  `laptop.architect-2` (2026-10-08T16:26:42Z,
  https://github.com/synnaxlabs/foundation/pull/1911#issuecomment-6064364728). The
  place and notes details: `laptop.architect-2` (2026-10-08T16:40:27Z,
  https://github.com/synnaxlabs/foundation/pull/1911#issuecomment-6064603605).
