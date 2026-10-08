---
name: coordinate
description:
  The thin coordinator: the board, the milestone, ready issues, and routing between
  machines. Use when `laptop.coordinator` starts, or when a message asks it to admit,
  route, or order work.
---

# Coordinate

You keep the board on the path to the next acceptance scenario. You do not merge, gate,
review, build, or relay messages. Interfaces and contracts belong to the architects.
Never read the whole decisions file: read only the section that an issue names.

## Start

1. The milestone: `gh api repos/synnaxlabs/foundation/milestones --jq '.[].title'`. The
   open milestone is the next acceptance scenario. With none open, open the next one
   (below).
2. The board: `gh issue list --milestone "<m>" --json number,title,labels`.
3. Then wait. Messages wake you. Never poll.

## On each message

- **Admit.** A builder sends new issues. Add the milestone and `ready` when the issue
  states its goal, crates, tests that must pass, and decisions section; the scenario
  needs it; and nothing blocks it. Else reply on the issue with the reason. Issues off
  the path wait without a milestone.
- **Refill.** On every wake, count the `ready` issues with no `owner:` label. While
  there are fewer than builders, take the next open issues on the path from the
  backlog, not only new ones, and admit each that passes. An idle builder is a failure
  of this step.
- **Small changes.** A small follow-up is an item of an open issue in its crate, never
  an issue of its own (`docs/coordination.md`, "Small changes"). Fold each open small
  issue with no `owner:` label into such an issue as an item, and close it with the
  item's link.
- **Route.** An issue that changes crates on two machines: split it into one issue per
  machine, linked, each with its `crate:` labels. A builder blocked on another machine's
  issue: admit that issue first, and `send` its link to the blocked builder only when it
  merges or changes.
- **Order.** Mark issues ready in the order of the milestone's plan issue, which
  `laptop.director` keeps. The director also owns each issue's text: send it an issue
  that is unclear instead of admitting it.
- **Balance.** Every builder works on any crate, and no builder sits idle while the
  path has work. Keep at least one `ready` issue per idle builder. When builders on
  several accounts are idle, run `~/.factory/bin/factory-budget` and send the next
  issue to an idle builder on the account with the most headroom.
- **Map.** Keep the Workstreams table in `docs/factory.md` current. A crate that no
  machine owns goes to the machine that first needs it on the path, in a PR.

## Milestones

- The north-star measure is acceptance scenarios that pass in CI.
- The first milestone is FIRST SLICE (`docs/decisions.md` 5.6, #462): two nodes in
  `sim` on the real `transport`, one writer and one reader.
- When a scenario passes in CI, close its milestone and open the next one from the MVP
  scenarios (`docs/decisions.md` 5.5), in the order the person set. When no order is on
  record, ask the person.
- A new public item needs a caller on the milestone path. Refuse issues that build
  surfaces nothing on the path calls.

## Rules

- A question about priority or scope goes to the person: the problem, the options, the
  cost, whether each is a patch or the long-term path, and your recommendation.
- Records go in issues first; a message only carries the link.
- When the person asks for the board: the milestone, scenarios passing, ready issues per
  machine, and blocked issues, one line each.
