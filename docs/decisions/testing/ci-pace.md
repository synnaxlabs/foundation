- **CI PACE (2026-10-06)** The ARM pool must not hold up the agents. The ARM workflow
  runs no loom step: loom is a software model, so the x86 `loom` job gives the same
  result. A PR run is cancelled by a newer push. A run on main is never cancelled while
  it runs; of the commits that merge during it, only the newest runs next. Each runner
  keeps its build in `$HOME/target/<runner>`, outside the workspace, and deletes it
  past 25 GiB. Dependencies build at opt-level 2 in the dev profile; workspace crates
  stay at opt-level 0 with their checks. A PR tests only the changed crates and their
  reverse dependencies (`cargo xtask affected`); main tests the whole workspace.
  Decided by the advisor (#782). The person said: "We need to make the agentic
  engineering the bottleneck, not CI".
