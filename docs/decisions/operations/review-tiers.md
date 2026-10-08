- **REVIEW TIERS (2026-10-06)** `reviewer` on every PR; `architecture` and `breaker` on
  every code PR; `performance` on hot paths, with measured numbers. A second round runs
  `reviewer` and `breaker` again on the fix commits only, with the earlier findings. A
  deferral in a risk crate needs the architect's explicit OK (the person, 2026-10-07:
  "YES"). 4 of the 5 worst escaped defects came in through a fix or a deferral that
  nothing checked again. Decided by the advisor under the quality delegation.
  Supersedes: BREAKER REVIEW.
