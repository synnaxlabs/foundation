- **PACE (2026-10-05)** `pace::Timer` ticks on a grid of deadlines at `start + n /
  rate`, from a `types::time::Rate`, and skips and counts the ticks a stall missed.
  It has one async `tick(&cancel::Token)`, with no blocking wait and no sleep, hybrid,
  or spin mode: precision belongs to the clock driver in `os` (#379). Decided by the
  `connector` builder in the plan on #237, after `/eb-review`; approved by the
  coordinator (#237). Supersedes: r12 A.3 `pace` modes and blocking wait.
