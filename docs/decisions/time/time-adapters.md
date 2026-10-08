- **TIME ADAPTERS** Neutral model `Measurement { at: local monotonic, offset, error }`.
  The estimator never knows what a source is. Each source is an adapter with its own
  loop. `node` builds the source table. Adapters probe for hardware and privileges. The
  same estimator serves device clocks in the connector library. Amended by ESTIMATE FIT:
  a device clock gives `Overlap` readings with a low edge, a high edge, or both, not
  `Measurement`s.
