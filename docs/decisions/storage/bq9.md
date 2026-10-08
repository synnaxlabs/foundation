- **BQ9** Re-index by changing `index` in the files. The old home seals the channel at
  its last accepted sample and records the seal with voters (which region: X39). The
  history "index A until T, index B from T" is runtime state in `mesh`; the spec keeps
  only the current index. Readers' `hub` joins the spans. No data moves. A connector
  rate change is not a re-index.
