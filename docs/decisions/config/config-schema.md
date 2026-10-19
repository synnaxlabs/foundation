- **CONFIG SCHEMA** One static `document::schema::Body` for each body is the only key
  list of its check, and `foundation docs` renders the same bodies, so the reference
  cannot state less than the check takes. `read::unknown(config, of, body)` refuses each
  attribute and block that `body` does not state. A `schema::Block` has one body for
  each form: `channel` has the forms `index` and `data`, and each gives `unknown` its
  own body. `config::SCHEMA` is a const of the seven core blocks in the order of their
  keywords, as in `KINDS`, and the file level gives `unknown` a body whose blocks are
  `config::SCHEMA`. The `connector` block states `kind` and `node`, and the kind states
  the rest. `connector::reader` gives `SELECT` and `BLOCK`, which the body of each kind
  that reads lists, and `reader::read` takes that body. Each `schema::Attribute` has a
  `Presence`: `Required`; `Unless`, required unless the body holds each attribute that
  it names in its place; `Default`, whose absence gives a value; or `Optional`, whose
  absence gives none, and the body summary states each rule that ties it to other
  attributes. `Presence::needed` states, from the attributes of a body, when it needs
  it. Each form of `config::SCHEMA` holds examples, which pin which attributes it takes
  and which it needs: the check takes each with no diagnostic, still with an attribute
  removed that it does not need, with a `document.missing-attribute` that names one that
  it needs when that one is removed, still with each attribute with a default removed at
  once, and with one `document.unknown-attribute` for a name that the body does not
  state. Each `Unless` names at least one attribute, and each name is an attribute of
  its body. Without the `kind` that picks its form, a block reads as the form whose
  `kind` has a default. No test reads the text of `value` or of a `Default`. The first
  block of an example is of its body, and of a block with more than one form, it holds
  only attributes of its form. Each attribute and block of a body is in at least one,
  or, for the body of a block inside another body, in an example of that outer body.
  `foundation docs` states each form of each core block, with no filter, and a test
  asserts that each has an example. A body that it does not state, such as
  `channel::ANY` or the file level, may have none. `foundation docs` gives each block of
  `config::SCHEMA` with its label and each form: its summary, one line for each
  attribute with its value, its `Presence` in words, and its summary, each example, and
  then each block inside it. Then it gives the body of each kind of the table that
  `ops::cli` takes, under its name, in the order of `Table::schemas()`, with the same
  parts. Then it gives each call of `secret::CALLS`, in its order, which is the list of
  calls that `Source::read` takes, each with an argument that `read` takes, and a test
  asserts that `Display` gives each call back. A terminal answer with a
  `document.unknown-attribute`, `document.unknown-block`, or
  `document.missing-attribute` ends with one line that names `foundation docs`. The JSON
  answer does not, and the first line of the MCP `instructions` names `docs`.
  `Kind::SCHEMA` is the body of each kind, with no default, and `Table::schemas()` gives
  each by name. An example of a kind is the body of its `connector` block without `kind`
  and `node`, which `Table::check(kind, None, example)` checks, and a test in `node`
  asserts that each kind of `kinds()` has one. Each test also walks each block inside an
  example, with the body of its form: the tests of the core examples in `config`, and
  those of each kind in `node`, over `kinds()`. Both run
  `document::testing::assert_examples`, behind `sim`, which checks each example, the
  example without each attribute that it does not need, the example without each
  attribute with a default, and each name of each `Unless`. The check that each
  attribute and block of a body is in an example, and the check of an example without an
  attribute that it needs, stay in each crate until #3349. `foundation docs` gives each
  kind example inside a whole `connector` block, labeled with the name of the kind, with
  `kind` and `node = "edge"`, so a copy is a block that a config takes. An example that
  holds `<<` keeps its lines with no indent, so a heredoc keeps its value. A test of the
  binary asserts that the body of each block, without `kind` and `node`, is the example,
  and a test in `ops` asserts that `config::check` takes as a file each such block of a
  table of test kinds, and #3350 adds the kinds of the binary. A test asserts that the
  fix of an unknown attribute or block at each site of each kind is the fix that
  `read::unknown` gives with the body of that site, so each kind refuses unknown items
  with the names that its `SCHEMA` states. Whether the check reads `SCHEMA` itself waits
  on #3334. `ops` gives the table to each run as an argument. `Scope` holds only what
  picks the files that a run reaches.
  Lost: the body alone in the output, which a person must wrap.
  Lost: a hand-written `reference.md` with a test that compares its names, whose values
  and defaults drift with no test; a `fn reference(&self) -> String` on each check,
  which keeps a second key list; a `default: Option<&str>`, whose `None` cannot state
  the influx targets or an absence that gives no value; an enum with `OneOf`, which
  grows into a rule language that the check already states; a list of alternative sets
  on `Body`, whose removal test is not exact for `placement`, which refuses with its own
  code, and which gives each body one more field; the pointer to `foundation docs` in
  the fix of each problem, which repeats it and changes the fix that `document` owns.
  Decided by `laptop.architect-2` at 2026-10-18T21:28:29Z
  (https://github.com/synnaxlabs/foundation/issues/3270#issuecomment-6255364169) and at
  2026-10-18T21:34:00Z
  (https://github.com/synnaxlabs/foundation/issues/3270#issuecomment-6255418341), and on
  the tests at 2026-10-18T22:19:34Z
  (https://github.com/synnaxlabs/foundation/pull/3277#issuecomment-6255858695). The OK
  of `Presence` and of the doc of `examples` at 2026-10-18T22:28:02Z
  (https://github.com/synnaxlabs/foundation/pull/3277#issuecomment-6255939421)
  supersedes "`Attribute` stays as written" of the first ruling. `Presence::needed`
  takes a `Map`, and the `disk` and `pool` of a node policy are each `Unless` the other,
  by `laptop.architect-2` at 2026-10-18T22:42:39Z
  (https://github.com/synnaxlabs/foundation/pull/3277#issuecomment-6256077638).
  `foundation docs`, `CALLS`, and the pointer line, by `laptop.architect-2` at
  2026-10-19T00:24:30Z
  (https://github.com/synnaxlabs/foundation/issues/3270#issuecomment-6257043728),
  which supersedes the `Body` text of the ruling at
  https://github.com/synnaxlabs/foundation/pull/3277#issuecomment-6256276766.
  The `connector` block, with a label, `kind`, and `node`, around each kind example in
  `foundation docs`, by `laptop.monitor` at 2026-10-19T02:57:06Z
  (https://github.com/synnaxlabs/foundation/pull/3332#issuecomment-6258666897).
  The table as an argument of each run, not a field of `Scope`, the test of the fix of
  an unknown attribute or block at each site of each kind, and the test of `foundation
  docs` through the binary, by `laptop.architect-2` at 2026-10-19T03:05:26Z
  (https://github.com/synnaxlabs/foundation/pull/3332#issuecomment-6258753211).
  The walk of an example in `document::testing`, behind `sim`, by `laptop.architect-2`
  at 2026-10-19T04:14:26Z
  (https://github.com/synnaxlabs/foundation/pull/3332#issuecomment-6259501604).
  `document::testing::assert_examples`, behind `sim`, by `laptop.architect-2` at
  2026-10-19T04:46:12Z
  (https://github.com/synnaxlabs/foundation/pull/3332#issuecomment-6259869702).
  Each example before the blocks inside its body, and an example that holds `<<` with no
  indent, in `foundation docs`, by `laptop.architect-2` at 2026-10-19T04:46:12Z
  (https://github.com/synnaxlabs/foundation/pull/3332#issuecomment-6259869702).
  The text of the two checks that stay in each crate until #3349, and of the `ops` test,
  by `laptop.architect-2` at 2026-10-19T05:02:10Z
  (https://github.com/synnaxlabs/foundation/pull/3332#issuecomment-6260052546).
  When a third table of the binary reaches `ops`, the tables go into one value of `ops`,
  by `laptop.architect-2` at 2026-10-19T03:05:26Z
  (https://github.com/synnaxlabs/foundation/pull/3332#issuecomment-6258753211).
