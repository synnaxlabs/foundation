---
name: crew
description:
  The daily quality pass over Foundation's main branch by the six crew agents. Use once
  a day from the coordinator session, or when asked to run the crew.
---

# Crew

1. `git fetch origin` and note the `origin/main` commit.
2. Launch the crew in parallel with the Agent tool, each against that commit:
   `code-quality`, `tests`, `architecture`, `performance`, `triage`, and `drift`.
3. Each agent returns findings. Drop duplicates and findings already filed
   (`gh issue list --label crew`).
4. File each remaining finding as an issue labeled `crew` and `crate:<name>`, owned by
   the session that owns the crate. One issue per finding.
5. When an agent finds a gap in its own rulebook, propose the rule to the person.
   Rulebooks are the agent files in `.claude/agents/`, and people own them.
