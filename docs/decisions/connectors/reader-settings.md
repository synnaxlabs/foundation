- **READER SETTINGS** `connector::reader::read` is the one reader of the S10 settings of
  an out connector: the `select` attribute and one `reader` block with `mode`
  (`hub::reader::Mode`, as a string or a reference) and `hold`. With no block the reader
  is complete and holds nothing. A second `reader` block is `document.repeated-block`,
  and `read` reads only the first, where a label is `document.label-count`. A negative
  `hold` is `document.negative-span` (READER RULES, #94; `laptop.architect-2`,
  2026-10-08T07:04:36Z,
  https://github.com/synnaxlabs/foundation/issues/1785#issuecomment-6054474145).
  Supersedes `config.repeated-block` and `config.label-count` of
  https://github.com/synnaxlabs/foundation/pull/1782#issuecomment-6051900967
  (2026-10-08T03:59:47Z), and `config.negative-span` for a `hold` of
  https://github.com/synnaxlabs/foundation/issues/895#issuecomment-6037207886.
  A `hold` in `latest` mode is `connector.latest-hold`, since only a complete reader
  holds.
  `read(config, keys, blocks)` takes the kind's own attributes and blocks and gives
  `document.unknown-attribute` or `document.unknown-block` for each other key it does
  not read (DOCUMENT KEYS), so a kind's key list does not change when `read` reads a new
  key. Decided by `laptop.architect-2` on #1153
  (https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152,
  2026-10-08 03:02 UTC, and
  https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051327019,
  2026-10-08 03:05 UTC). A kind that lists `select` in its attributes or `reader` in its
  blocks is a defect in the kind, and `read` panics (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/pull/1782#issuecomment-6052200724, 2026-10-08
  04:25 UTC). Supersedes the `KEYS` part of
  https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152 and
  https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051327019
  (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/pull/1782#issuecomment-6051900967, 2026-10-08
  03:59 UTC).
  A reader always has its connector's name, and the `reader` block has no `name`: a
  `name` in it is `document.unknown-attribute`. Connector names are unique (CONNECTOR
  BLOCK), so two connectors never share a reader, and plan needs no check for it.
  `kind::Context::reader` (#1731) opens the reader under the connector's name. Lost:
  keep `name` and refuse a repeated reader name at plan, a new surface for a choice that
  nobody uses (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/1807#issuecomment-6057222444,
  2026-10-08T09:49:27Z). Supersedes `name: Option<Name>` of `reader::Settings` in
  https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152, and the
  `Settings::name` of `None` for a reader with no `name` of
  https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6052681089
  (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/pull/1794#issuecomment-6053214653, 2026-10-08
  05:43 UTC). Supersedes the ad hoc reader and `connector.unnamed-hold` of
  https://github.com/synnaxlabs/foundation/issues/1153#issuecomment-6051297152
  (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/issues/1736#issuecomment-6052555898, item
  7, 2026-10-08 04:53 UTC).
