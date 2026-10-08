# Decisions

The source of truth for Foundation's design. Every locked decision, where each data
structure lives, the crate map, and what is still open.

Each record is one file in the folder of its topic, such as
`transport/stream-wire.md`. The file name is the record's ID in lower case, without
its text in parentheses, with `-` for each group of other characters. When a file in
the folder already has that name, the new file keeps the text in parentheses, and the
old file keeps its name. The folders are the index, and no file lists the records. To
find a record by its ID, use `grep -rl '^- \*\*READER RULES' docs/decisions/`.

The person decided on 2026-10-08 at 19:36 UTC to split `docs/decisions.md` into one
file per record, to stop the merge conflicts on that file
(https://github.com/synnaxlabs/foundation/issues/1980). `laptop.director` decided the
folders and the file names (https://github.com/synnaxlabs/foundation/pull/1983).

The other files:

- `where/`: where each data structure is defined and stored.
- `contradictions/`: the contradictions in the design, and how each was resolved.
- `crate-map.md`: the crate map. `xtask/src/map.rs` must match it.
- `open/`: the open items, the parameters for experiment, the MVP, and the first
  phase.
- `retired.md`: each retired entry, once, and the entries that replace it.

How to read a record:

- IDs come from the design interview (A7, BQ6) or its labels (REGION LOCKED). Study
  decisions carry the study number (R9-D4, R12-4, R13-5). The studies are in
  `docs/research/`.
- "Supersedes" names an earlier entry that no longer holds. `retired.md` lists every
  retired entry once.
- The person delegated these areas to the design session: quality, memory and
  performance, failover, names, delivery and wire internals, and "decide the best
  architecture". Decisions made under a delegation are as binding as the rest.
- A **region** is the part of the name tree that one voter set governs.
- To add a decision, add its file to the folder of its topic. To change a decision,
  follow "Interface changes" in `docs/coordination.md`. A change to a locked decision
  needs the person.
