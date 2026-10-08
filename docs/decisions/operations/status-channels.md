- **STATUS CHANNELS (2026-10-06)** `node::status::TABLE` is the fixed set of a node's
  status channels: `clock.status` (`U8`: 0 unsynced, 1 synced, 2 holdover),
  `clock.offset` (`Span`), and `clock.error` (`Span`; an unknown error is the bound of
  `estimate::Measurement::unknown`). An unsynced clock gives no offset or error. Each
  table entry maps the pulled status to its value. A pure `Collector` pulls each value
  from a reader that its crate gives; no crate calls `node`. Lost: each crate pushes
  status events to a sink (BQ11b locks pull). Decided in #728.
