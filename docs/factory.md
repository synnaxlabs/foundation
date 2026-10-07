# Factory

Fifteen Claude sessions build Foundation on three machines. This file says who runs
where, how work flows, and how it merges. `docs/coordination.md` covers issues, PRs,
interface changes, and messages.

## Sessions

Every session runs Opus 5.5 in auto mode. Its engineer opens it as a cmux workspace on
the person's laptop and runs its launcher line there: `cmux new-workspace --name <role>
--command '<line>'` for the laptop, `cmux ssh ubuntu@<box> --name <role> --command
'<line>'` for a box. No tmux.

| Name | Machine | Engineer | Skill | Launcher line |
| --- | --- | --- | --- | --- |
| `laptop.coordinator` | laptop | the person | `/coordinate` | `~/.factory/bin/factory-agent laptop.coordinator` |
| `laptop.architect` | laptop | the person | `/architect` | `~/.factory/bin/factory-agent laptop.architect` |
| `laptop.integrator-1` | laptop | the person | `/build` | `~/.factory/bin/factory-agent laptop.integrator-1` |
| `laptop.integrator-2` | laptop | the person | `/build` | `~/.factory/bin/factory-agent laptop.integrator-2` |
| `laptop.monitor` | laptop | the person | none | none: it runs already |
| `box1.builder-1` | box1 | Ronaldo | `/build` | `~/.factory/bin/factory-agent box1.builder-1` |
| `box1.builder-2` | box1 | Ronaldo | `/build` | `~/.factory/bin/factory-agent box1.builder-2` |
| `box1.builder-3` | box1 | Ronaldo | `/build` | `~/.factory/bin/factory-agent box1.builder-3` |
| `box1.builder-4` | box1 | Ronaldo | `/build` | `~/.factory/bin/factory-agent box1.builder-4` |
| `box1.red-team` | box1 | Ronaldo | `/red-team` | `~/.factory/bin/factory-agent box1.red-team` |
| `box2.builder-5` | box2 | Sergio | `/build` | `~/.factory/bin/factory-agent box2.builder-5` |
| `box2.builder-6` | box2 | Sergio | `/build` | `~/.factory/bin/factory-agent box2.builder-6` |
| `box2.builder-7` | box2 | Sergio | `/build` | `~/.factory/bin/factory-agent box2.builder-7` |
| `box2.connector` | box2 | Sergio | `/build` | `~/.factory/bin/factory-agent box2.connector` |
| `box2.red-team` | box2 | Sergio | `/red-team` | `~/.factory/bin/factory-agent box2.red-team` |

The launcher sources `~/.factory/env`, so `gh` and `git` act as the GitHub App
`synnax-foundation-factory[bot]` (one private key per machine). It makes the worktree
`~/Desktop/synnaxlabs/foundation-wt/<role>`, sets `FACTORY_NAME`, and starts Claude
Code with `--name <role>`, the factory mod, and the role's skill. The architect runs at
`xhigh` effort, the others at `high`. The monitor watches factory efficiency and
performance.

A builder works one issue per context. When its PR merges, it calls the factory mod's
`next` tool, which runs `/clear` and then `/build` by itself.

## Workstreams

Each machine is home to a set of crates. A home is not a limit. Any builder takes a
`ready` issue in any crate, so work goes to idle builders and every account spends its
budget. One issue is in progress per crate. The coordinator keeps this table current.

| Machine | Host | Crates |
| --- | --- | --- |
| laptop | the laptop, 16 cores | `node`, `config`, `config-hcl`, `document`, `ops`, `acceptance` |
| box1 | `foundation-factory`, 64 vCPU | `hub`, `control`, `delivery`, `home`, `mesh`, `raft`, `spec`, `access`, `blob`, `buffer`, `replica`, `block`, `ring`, `types`, `codec`, `wire`, `counting` |
| box2 | `foundation-factory-2`, 32 vCPU | `transport`, `clock`, `estimate`, `sim`, `env`, `os`, `secret`, `connector`, `connector-<kind>`, `daqmx-stub` |

The risk crates are `raft`, `buffer`, `delivery`, `block`, `ring`, `codec`, `wire`,
`home`, and `replica` on box1, and `transport` on box2. The red-teams aim at them.

## Milestones

The unit of work is the next acceptance scenario, one GitHub milestone. Only issues on
its path get `ready`. Each builder has one issue in progress and keeps two ready. A new
public item needs a caller on the path. The north-star measure is acceptance scenarios
that pass in CI.

## Two lanes

- **Day lane (watched).** Open questions, public surfaces, decisions, and risk-crate
  code. Hours follow each engineer.
- **Night lane (unwatched).** Before they leave, the engineer runs `/clear` and
  `/build night` in each builder with night work and no open day issue, and
  `/red-team night` in the red-team. A builder takes only issues the architect labeled
  `night` during the day:
  the contract is on `main` and compiles, the acceptance tests are named and present, no
  decision is open, and the work stays in one crate of the session's machine. The model
  case is `box2.connector`: a connector against a locked `hub` contract with a device
  simulator.
- **Machine work at night.** The red-teams run simulation campaigns, fuzz, and mutants.
  Each failure becomes a day-lane issue with a reduced repro.
- Night PRs merge through the queue like day PRs.

## Messages

- Same machine: the built-in `SendMessage`, to the role (`builder-1`).
- Across machines: the factory mod's `send` tool, to the full name (`box1.builder-1`).
  It checks the roster, adds an ID, and handles the ack.
- A message points at an issue or a PR. Write the record there first.
- No session polls. Never `/loop` or `ScheduleWakeup` to wait. A message, or the exit of
  the session's one background command, wakes it.

## Merge path

- A ruleset on `main` requires the CI checks, the merge queue, and code-owner review,
  with zero other approvals.
- After its local gate and review, the author runs `gh pr merge <n> --auto`. The queue
  tests each PR on top of `main` and merges it.
- Code owners (`.github/CODEOWNERS`): the person owns `oracles/` (except the fuzz
  inputs in `oracles/fuzz/`), `.github/`, `CLAUDE.md`, and `.claude/`. Everything else
  merges when its checks pass.
- The architect reviews every public-interface or crate-dependency change before the
  person.
- Branches start with the machine: `<machine>/<issue>-<slug>`, for example
  `box1/462-hub-route`.

## GitHub is the record

- Tasks, decisions, and approvals live in issues, PRs, and the repo, never only in a
  message or a session's context.
- An approval is a link to the person's or engineer's own comment, with its UTC time.
  Never record a paraphrase as an approval.
- A new decision says "Supersedes <link>". Decision changes ship in their own PRs, never
  inside code.
- Each issue's last state comment says what is done, the next step, and open questions.

## Models

Opus 5.5 everywhere. Fable only on an issue the person or the architect labels
`model:fable`. The weekly `code-quality` and `drift` agents run on Sonnet; search
subagents run on Haiku.
