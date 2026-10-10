- **ENTRY TAGS (#191)** Each entry of an index log has a tag (S4) that says what its
  bytes hold: `DATA` 0 (STORED BODY), `HANDOFF` 1 (HANDOFF RECORD). A new kind of
  record takes the next free value here. The buffer does not read the tag.
  Decided by the `write-path` builder; approved by the coordinator (#191).
