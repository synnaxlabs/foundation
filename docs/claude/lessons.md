# Architecture lessons

Each lesson came from a design decision that a person corrected. Apply each one
before you propose a design, not after someone points it out.

## Library, not framework

The code with the edge cases owns its control flow and composes small shared
components. A framework that owns the loop and calls plug-in hooks is the exception,
for places where nothing varies. When a boundary has the shape "the lower part calls
the upper part's hooks", test the inverted version first. Give the common case a
ready-made composition built only from the same public pieces.

Evidence: a connector design where a shared actor owned the loop and each kind
implemented device hooks was rejected. Hardware has edge cases: when devices start, how
reads are paced, software against hardware timing, socket settings, extra threads. The
Synnax EtherCAT engine runs its own real-time thread behind a `read` hook. Telegraf
added `ServiceInput` beside `Gather`. Debezium runs its own reader thread and queue
behind Kafka Connect's `poll`. OpenTelemetry's `scraperhelper` is an optional helper,
not a required base.

## Neutral model at the boundary

When a component consumes something that comes in interchangeable forms (file syntaxes,
transport carriers, time sources, secret stores, SDK languages), the core works on one
neutral model, and each form is an adapter that reads or writes it. Ship one default
adapter first.

- The model carries everything the richest form needs (types, case, source positions),
  never only what the weakest form can say. Viper lowercases every key so sources can
  merge, and it drops source positions.
- Parts that only one form could express get their own grammar, so no adapter is
  privileged. Calculation expressions are strings with their own grammar, like PromQL
  in YAML or CEL in Kubernetes.

Applied in Foundation: the config `Document` with HCL as the first front end; time
`Measurement` with sources as adapters; the transport session model with QUIC, TLS over
TCP, relay, and diode carriers; secret stores.

## The naming tell

A compound name that repeats a responsibility (`home::ControlGate`) means the module
holds a second job that wants its own module (`control::Gate`). Split until the names
get simple. Hunt for these in every crate map and review.

## Dependency direction

Before you add a structure, draw its edges: what it points at, and what points at it.
Keep the core item minimal. Prefer a setting as a selector over many items (a policy)
to a field on each item. Prefer reusing a core concept to adding a side structure:
quality became an ordinary channel that many channels can point at, like an index.

## Policies never create channels

A policy (retention, compression, reduction, access) selects existing channels and
changes how they are handled. Anything that creates a channel (a connector, a
calculation) is an explicit definition, so it shows up in `plan`.

## A status channel is a published copy

A value inside the core is the truth. Its status channel is a copy for people, agents,
and outside tools. Core decisions (failover, fencing) use the internal value and never
read their own status channels back. Rebalancing and other automation are outside
controllers that read status channels and act through `plan` and `apply`.

## Seating

For every component, ask how deep it reaches into other components' internals, and
whether a clean boundary can separate it. When it cannot, make the trade explicitly and
write it down. Replication is the example: it is a separate `replica` component that
uses two narrow calls into the home, not a reader inside `hub`.

## Scope by the future, not the first caller

Judge a build-or-use choice by every user the part will have, not by its first caller.
Crate count and binary size are cheap, and they never tip the trade. Never hand-write a
protocol, parser, or transport when a mature library meets our needs and lets us inject
I/O and time. Our own build needs evidence that the library fails, as HCL READER has:
`hcl-edit` overflowed the stack on deep nesting and misread a number (#85). When a
library fits, our own code goes in the adapters, not in the protocol.

Name the future users, and count only the ones on record: an entry in
`docs/decisions.md`, an open issue, or a Synnax feature. The future scope picks the part
we build on. It does not mean we build the future features now, and it does not excuse a
trait with one speculative implementation.

Evidence: #341 needed one InfluxDB POST. The first plan was a sans-I/O HTTP/1.1 module
in `connector-influx` on `httparse`, to avoid new crates and a larger binary. A person
rejected it: a general HTTP connector (like the one in Synnax), alarms, webhooks, and
remote write all need HTTP. R7 now holds one `hyper` client over the `env` network seam.
`hyper` takes any I/O type and an injected timer, so `sim` stays deterministic.

## The repo is the memory

Sessions compact, crash, and get replaced. A decision that lives only in a session's
context is lost. Write it into `docs/` in the same PR that depends on it.

## One target dir for each worktree

Cargo names the build of a workspace crate by its path from the workspace root, so the
same crate in two worktrees gets one artifact in a shared `CARGO_TARGET_DIR`. Cargo
rebuilds it only when a source file is newer than the artifact. A worktree whose
sources are older then runs a test binary or `xtask` built from another worktree, and
a local gate reports the result of the wrong tree. Each worktree uses its own target
dir: cargo's default, `target/` in the worktree. Touching the changed sources is a
patch, not the fix.

Evidence: in the #734 worktree, `cargo test -p transport` ran 194 tests with 3
failures. Those are the counts of #727, built in another worktree. A fresh copy of
#734 passed 190. `cargo xtask oracles` in one worktree compiled the crates of another,
because `xtask` fixes the workspace root when it compiles (#730).
