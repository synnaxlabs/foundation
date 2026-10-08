- **GROUPS DROPPED + CONNECTOR = TASK** A connector is a task: one index for reading
  (one rate, one clock, one writer) or one reader for writing. There are no groups.
  Connectors that name the same endpoint share it through a library component that owns
  the handle once per node. The kind's checker rejects combinations the hardware cannot
  do. Supersedes: C3 REFINEMENT groups.
