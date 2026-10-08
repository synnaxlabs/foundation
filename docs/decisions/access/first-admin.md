- **FIRST ADMIN (2026-10-08)** A node that starts a new mesh has an empty spec, and
  under BQ12 only a key that the spec names can sign an apply. So the first `foundation
  start` of that node, on an empty data directory, creates the spec with one admin
  subject. It writes the admin's private key into the data directory, and only the user
  who started the node can read the key. The CLI on the same host signs with that key,
  so the first `apply` needs no key step. A node that joins by ticket (BQ11a) joins a
  mesh that has a spec, so it creates none. BQ12 holds as written: each node checks each
  apply, the first one too, against a key in the spec. Lost: the first apply from any
  local process, because any local user could then take the node. Also lost: an admin
  public key given before the first start, a step before the first use. Decided by the
  person ("Yes, I approve."), relayed by `laptop.monitor` at 2026-10-08T02:43:43Z:
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6051096981. The
  #1744 plan names the subject, its access policy, and the key file, as
  `laptop.architect-2` and `laptop.architect` decided (2026-10-08T02:46:51Z,
  https://github.com/synnaxlabs/foundation/pull/1759#issuecomment-6051130026). The
  question was about a node whose spec is empty. So `laptop.architect` decided the limit
  to a node that starts a new mesh, and the sentence on a node that joins
  (2026-10-08T02:57:01Z,
  https://github.com/synnaxlabs/foundation/pull/1760#issuecomment-6051238643). It also
  decided that the #1744 plan names how a first start tells a new mesh from a join
  (2026-10-08T02:59:40Z,
  https://github.com/synnaxlabs/foundation/pull/1760#issuecomment-6051265693).
  `spec::founding::create(admin)` gives the first admin: the subject `@admin` at
  `@admin.@subject`, which holds the admin's public key, and the access policy `@admin`
  at `@admin.@access`, which allows the subjects `@admin` every action on `**` with no
  authority, so its writes cap at `Authority(0)` (ACCESS BLOCK). A policy in a file can
  give the admin more. Their labels are reserved, so no file holds them: `Kind::key`
  refuses a reserved label, and `Kind::label` gives one only at a founding key: a key
  of `create`, or one that an earlier build made (`laptop.architect`,
  2026-10-08T12:51:04Z,
  https://github.com/synnaxlabs/foundation/pull/1880#issuecomment-6060256808). A
  private table in `spec::founding` holds the founding labels by kind. A later build can
  add an entry and never removes one, as a committed spec holds the keys of an earlier
  build. Supersedes "only a subject or an access policy can have a reserved label"
  (`laptop.architect-2`, 2026-10-08T10:56:38Z,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6058336549), as a
  client's plan could then add a signing subject such as `ops.@x` that no plan shows
  (#1877;
  `laptop.architect`, 2026-10-08T12:36:57Z,
  https://github.com/synnaxlabs/foundation/pull/1862#issuecomment-6060015621). A
  definition whose label (`definition.kind().label(key)`) is reserved is
  Foundation's, and `plan` leaves it out. `access::Rules` finds a subject by its label,
  so it admits `@admin` (SUBJECT KEYS). Lost: `Kind::key` takes a reserved label behind
  a flag, so `node` writes the definitions and `config` can make a reserved key by
  mistake; `spec::key::reserved(key)`, which needs a list of every kind that a new kind
  can miss, and gives `plan` no label or kind to print. Decided by `laptop.architect-2`
  (2026-10-08T06:01:36Z,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6053458318), with
  the rule and its lost option at 2026-10-08T10:56:38Z
  (https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6058336549); no
  authority decided by `laptop.architect` (2026-10-08T06:11:30Z,
  https://github.com/synnaxlabs/foundation/issues/1744#issuecomment-6053599101). The
  removal of `spec::key::reserved` and the public `Definition::kind` approved by
  `laptop.architect` (2026-10-08T11:11:39Z,
  https://github.com/synnaxlabs/foundation/pull/1862#issuecomment-6058589907).
  A client's plan never changes a founding definition. Else a subject that may apply
  in the root region can change or remove the admin's key or policy, and no later plan
  shows it. The `ops` apply refuses a plan with a change at a reserved label through
  `config::plan::Plan::definitions` (`Mismatch`, PLAN FILE), before it proposes
  anything. `ops` has no label check of its own, as that is a second guard of one
  rule. Lost: the check in `Mesh::apply`, a rule of a client's input and not of region
  state; no `apply` grant on a reserved name in `access::Rules::grant`, which gives one
  action an exception away from the code that reads the plan. Decided by
  `laptop.architect` (2026-10-08T12:36:57Z, part 2 of
  https://github.com/synnaxlabs/foundation/pull/1862#issuecomment-6060015621), changed
  by `laptop.architect` (2026-10-08T22:57:18Z,
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6070697149) and
  `laptop.architect-2` (2026-10-08T19:20:33Z,
  https://github.com/synnaxlabs/foundation/pull/1970#issuecomment-6067332179).
