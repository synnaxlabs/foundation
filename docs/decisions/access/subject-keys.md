- **SUBJECT KEYS (2026-10-08)** A person, an agent, or a program is a
  `spec::subject::Subject` at `<name>.@subject`, which holds its Ed25519 public keys
  (`types::ed25519::PublicKey`): at least one, each distinct, sorted by their bytes so
  the order of a file does not change the definition. The `subjects` selector of an
  access policy matches `<name>`, not the tree key, and #1747 makes `access::admit` read
  `<name>.@subject`. A connector has no subject definition: it stays at its own name
  with no keys. The encoding is tag 10, a count, and 32 bytes for each key in ascending
  order; `decode` checks the count against the bytes left before it allocates, and
  refuses an empty list, keys out of order or equal, and a key of small order. Lost: the
  subject at its plain name, which takes that name from a channel or a connector and
  allows no children. Decided by `laptop.architect-2` at 2026-10-08T03:15:41Z
  (https://github.com/synnaxlabs/foundation/issues/1755#issuecomment-6051435217). In a
  file, a `subject` block has one attribute, `keys`: the line of an OpenSSH `.pub` file,
  or a list of them, as `allow` takes one action or a list. `config` keeps the key, not
  the comment. It reads the line with `ssh-key` (`laptop.architect-2`,
  2026-10-08T17:21:06Z,
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6065299343), and
  refuses a base64 word that differs from the one that `ssh-key` writes for the key,
  since `from_openssh` accepts a key length field over 32 when 32 bytes follow. So one
  key has one base64 form (`laptop.architect-2`, 2026-10-08T17:59:19Z,
  https://github.com/synnaxlabs/foundation/pull/1943#issuecomment-6065948208). A
  string that holds `PRIVATE KEY` (the OpenSSH, PEM, and RFC 4716 forms) or
  `PuTTY-User-Key-File` (a `.ppk` file) gives `config.private-key`, whose message
  quotes none of the value. A `.pub` line whose
  comment holds `PRIVATE KEY` gets that alarm too, because a missed private key costs
  more. A base64 body with no header lines gets it too: `b3BlbnNzaC1rZXktdjEA` starts
  each OpenSSH body, and `BQYDK2VwBCIE`, `MAUGAytlcAQi`, and `BgMrZXAEIgQg` are the
  algorithm and key header (`30 05 06 03 2B 65 70 04 22 04 20`) of each Ed25519 PKCS #8
  body, v1 and v2, at each of its offsets modulo 3, which the length of the body moves
  (#1886 round 1, 2026-10-08T13:34:39Z,
  https://github.com/synnaxlabs/foundation/pull/1886#issuecomment-6061047969, and round
  2, 2026-10-08T14:14:16Z,
  https://github.com/synnaxlabs/foundation/pull/1886#issuecomment-6061802143, under the
  rule of `laptop.architect-2` at 2026-10-08T12:06:26Z that the marks are text that only
  a private key holds,
  https://github.com/synnaxlabs/foundation/pull/1858#issuecomment-6059482219).
  Supersedes the mark `MC4CAQAwBQYDK2VwBCIE` of that rule. Each is whole 3-byte groups
  at an offset of whole groups, so the bytes around it do not change it. An Ed25519
  public key (`MCowBQYDK2VwAyEA`) does not hold it. Lost: a mark for the body of another
  algorithm, such as RSA (`MIIE...`), whose start is also the start of a certificate. As
  OpenSSH reads a `.pub` line, the comment is the rest of the line, so a line with a
  second key in its comment gives the first key. Decided by `laptop.architect-2` at
  2026-10-08T12:06:26Z
  (https://github.com/synnaxlabs/foundation/pull/1858#issuecomment-6059482219).
  `config::check` first looks at each string of each Document, in any block
  (keywords, labels, keys, and values at any depth). When one holds a private key, it
  gives only these alarms, one for each such string, and runs no other check, so no
  other problem can quote the key. Lost: the alarm only in the `subject` block, which
  misses a key in a connector config; the alarm with the other problems kept, because
  each check must then not quote a value. Decided by `laptop.architect-2` at
  2026-10-08T10:33:30Z
  (https://github.com/synnaxlabs/foundation/pull/1858#issuecomment-6057957877). A PEM
  or RFC 4716 public key gives `config.bad-public-key`. A message quotes at most the
  first word of a value: an algorithm name from a closed table of OpenSSH key types,
  with `{:?}`. The form of one line or a list, the first-word rule, and the marks
  of a private key are from `laptop.architect-2` at 2026-10-08T06:42:17Z
  (https://github.com/synnaxlabs/foundation/pull/1823#issuecomment-6054095085), which
  supersedes item 1 of
  https://github.com/synnaxlabs/foundation/issues/1755#issuecomment-6053298142
  (`laptop.architect-2`, 2026-10-08T05:49:53Z). That ruling decided the PEM rule and
  supersedes change 2 of
  https://github.com/synnaxlabs/foundation/issues/1755#issuecomment-6051435217
  (2026-10-08T03:15:41Z). `config` refuses a `subject` at the name of a `connector`
  (`config.subject-is-connector`)
  (https://github.com/synnaxlabs/foundation/issues/1755#issuecomment-6051435217), with a
  note at the connector (`laptop.architect-2`, 2026-10-08T06:58:26Z,
  https://github.com/synnaxlabs/foundation/pull/1823#issuecomment-6054362063), in any
  ASCII case (`laptop.architect-2`, 2026-10-08T07:02:08Z,
  https://github.com/synnaxlabs/foundation/pull/1823#issuecomment-6054428920). The read
  moves to `ssh-key` in the PR of #337 that first prints a key's fingerprint. Decided
  by `laptop.architect-2` at 2026-10-08T06:42:17Z
  (https://github.com/synnaxlabs/foundation/pull/1823#issuecomment-6054095085). The
  person approved `ssh-key` 0.6.7 at 2026-10-08T16:58Z
  (https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6064910986).
  `config::openssh` keeps its own checks and messages: the first-word table and one
  line before `ssh-key`, and small order after it (`laptop.architect-2`,
  2026-10-08T17:21:06Z,
  https://github.com/synnaxlabs/foundation/issues/337#issuecomment-6065299343).
  `access::Rules` keeps each subject by its label, which
  `spec::definition::Kind::label` gives for its tree key. `admit` and `verify` look up
  the hello's subject and build no key, so only `spec` holds the key form, and
  `@admin`, a label that `Kind::key` refuses, can sign (FIRST ADMIN). A subject with no
  definition gives `Error::Unknown`. `Kind::label` is public: `access` calls it, and
  the `plan` of #1744 PR 1b and `export` will call it. Decided by `laptop.architect` at
  2026-10-08T11:00:08Z
  (https://github.com/synnaxlabs/foundation/pull/1862#issuecomment-6058397812).
  `Rules::new` panics at a subject definition at a key that gives no label, which its
  precondition excludes (`laptop.architect`, 2026-10-08T13:40:56Z,
  https://github.com/synnaxlabs/foundation/pull/1880#issuecomment-6061170874; built
  for #1890). Supersedes the skip of
  https://github.com/synnaxlabs/foundation/pull/1862#issuecomment-6058589907
  (2026-10-08T11:11:39Z).
  `Rules::new` takes only trees with no problem from
  `spec::region::check` at their prefix, and checks nothing itself: `Mesh::spec` gives
  only such trees (SPEC CHANGE, #1741), and `node` builds `Rules` only from it (#1744).
  Lost: a governs check in `Rules::new`, a second guard that puts the rule of
  `spec::region` in a second crate; a checked tree type, which proves each tree but not
  that the trees are the regions of one mesh (#1882; `laptop.architect`,
  2026-10-08T12:59:50Z,
  https://github.com/synnaxlabs/foundation/issues/1882#issuecomment-6060416943). The
  first ruling kept each subject by `<name>` (`laptop.architect`,
  2026-10-08T06:56:19Z,
  https://github.com/synnaxlabs/foundation/issues/1747#issuecomment-6054321636). The
  tree key, which `admit` built with `Kind::key`, was decided by `laptop.architect` at
  2026-10-08T08:10:33Z
  (https://github.com/synnaxlabs/foundation/pull/1834#issuecomment-6055629911), with
  the lost option of a public `Kind::label` that only `access` calls. The ruling of
  11:00:08Z supersedes ruling 1 (the tree key) of
  https://github.com/synnaxlabs/foundation/pull/1834#issuecomment-6055629911.
