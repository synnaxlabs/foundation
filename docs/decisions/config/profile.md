- **PROFILE (2026-10-20)** The profile is the HCL file of a program's host that names
  its subject and pins the public key of each node that it connects to. `sdk-python`
  reads it at `FOUNDATION_PROFILE`, else `~/.foundation/profile.hcl` (the plan, "The
  profile",
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279507257). It
  has three optional attributes and one block kind:

  ```hcl
  subject     = "plant.alice"          # a subject name
  private_key = "alice.key"            # the key file of the subject
  strict      = true                   # refuse a node that no source pins

  node "10.0.0.5:7400" {
    public_key = "ssh-ed25519 AAAA... node@host"
  }
  ```

  A relative `private_key` path is relative to the directory of the profile, and a path
  that starts with `~/` to the home directory; each other path is as written.
  `sdk-python` joins it (`laptop.architect-2`, 2026-10-20T01:11:29Z, item 3 of
  https://github.com/synnaxlabs/foundation/pull/3472#issuecomment-6280967938).
  `config::profile::decode(&Document)` gives `config::profile::Profile`: the subject,
  the key file path, `strict` (default `false`), and a map from each address (`ip:port`)
  to its Ed25519 public key. A `node` block holds only the public key
  (`laptop.architect-2`, 2026-10-19T23:49:24Z, item 1 of
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279895005). Each
  problem is a `Diagnostic` with its fix: an unknown attribute or block, a wrong type, a
  bad name, an empty path, a `private_key` with no `subject` (item 3 of 6280967938), a
  `node` label that is not `ip:port`, a key line that is not `ssh-ed25519`, and a second
  block of one address, with a note at the first. A string that holds a private key
  gives only `config.private-key`, as in a config file (SUBJECT KEYS), with the fix to
  write the path of its key file as `private_key`, or, in a `node` block, to use the
  line that `foundation profile` prints (`laptop.architect-2`, 2026-10-20T01:51:26Z,
  item 1 of https://github.com/synnaxlabs/foundation/pull/3472#issuecomment-6281458911;
  the fix, 2026-10-20T02:08:10Z, item 3 of
  https://github.com/synnaxlabs/foundation/pull/3472#issuecomment-6281670865; the fix in
  a `node` block, 2026-10-20T02:36:54Z, item 3 of
  https://github.com/synnaxlabs/foundation/pull/3472#issuecomment-6282027387). A pin
  whose key holds such text also gives it: a key holds it with a chance under 10^-19,
  and an exception would pass a private key in the comment of the line
  (`laptop.architect-2`, 2026-10-20T01:53:14Z,
  https://github.com/synnaxlabs/foundation/pull/3472#issuecomment-6281480378).
  `config::profile::node(address, public_key)` gives the `node` block of a pin as a
  `document::Block`, and the program writes the file with `config_hcl::update`, so each
  other byte of the file stays (`laptop.architect-2`, item 2 of 6280967938). `decode`
  reads the block back, with the flow label of a V6 address as 0. The key file is the
  OpenSSH private key file of an Ed25519 key with no passphrase, as `ssh-keygen -t
  ed25519 -N ""` writes it. `config::openssh` reads and writes it with `ssh-key`, and
  refuses another algorithm by its name (an ECDSA key as `ECDSA`, which `ssh-key` reads
  only with its `ecdsa` feature), an encrypted key, a public key that is not the key of
  the private key (`Mismatch`), a comment that is not UTF-8 (`Comment`), and a text that
  is not a key file. It reads each such file that `ssh-key` 0.6.7 reads, also one with a
  body of any width or with white space, any bytes on the lines after the end mark, CRLF
  line ends, or no line end after the end mark (the bytes, `laptop.architect-2`,
  2026-10-20T02:30:05Z,
  https://github.com/synnaxlabs/foundation/pull/3472#issuecomment-6281938918; on the
  lines, 2026-10-20T03:11:15Z,
  https://github.com/synnaxlabs/foundation/pull/3473#issuecomment-6282446293). It
  refuses some files that OpenSSH reads: a comment that is not UTF-8 (`Comment`), and,
  as `Format`, padding of a block or more, options of the `none` KDF, and a NUL in the
  body (`Format` with "the file is not an OpenSSH private key as `ssh-keygen` writes
  it": `laptop.architect-2`, 2026-10-20T02:56:10Z,
  https://github.com/synnaxlabs/foundation/pull/3472#issuecomment-6282260036;
  `laptop.director`, 2026-10-20T02:19:48Z,
  https://github.com/synnaxlabs/foundation/pull/3472#issuecomment-6281813278;
  `laptop.architect-2`, 2026-10-20T02:36:54Z, item 2 of
  https://github.com/synnaxlabs/foundation/pull/3472#issuecomment-6282027387; the line
  ends and white space, `laptop.architect-2`, 2026-10-20T02:08:10Z,
  https://github.com/synnaxlabs/foundation/pull/3472#issuecomment-6281670865, in place
  of item 2 of 6281458911; and 2026-10-20T01:53:14Z, item 4 of 6281480378). Each refusal
  is a `config::openssh::Unreadable`: `Display` gives the message, and `code` and `fix`
  give the other parts, as `ops::Failure` takes them:

  | Variant | Code | Fix |
  | --- | --- | --- |
  | `Format` | `config.key-format` | Use the private key file that `ssh-keygen -t ed25519 -N ""` writes, not its `.pub` file |
  | `Algorithm` | `config.key-algorithm` | Make a new key with `ssh-keygen -t ed25519 -N ""`, and add its `.pub` line to `keys` of the `subject` block |
  | `Passphrase` | `config.key-passphrase` | Use a key file with no passphrase, as `ssh-keygen -t ed25519 -N ""` writes it |
  | `Mismatch` | `config.key-mismatch` | Make a new key with `ssh-keygen -t ed25519 -N ""`, and add its `.pub` line to `keys` of the `subject` block |
  | `Comment` | `config.key-comment` | Give the key a comment in UTF-8 with `ssh-keygen -c -C <comment> -f <file>` |

  No fix removes a passphrase, since that lowers the security of the user's own SSH key.
  The node knows a subject only by the `.pub` lines in `keys` of its `subject` block, so
  a new key also goes there (`laptop.architect-2`, 2026-10-20T02:11:21Z,
  https://github.com/synnaxlabs/foundation/issues/3490#issuecomment-6281710183;
  `Comment`, 2026-10-20T02:36:54Z, item 1 of
  https://github.com/synnaxlabs/foundation/pull/3472#issuecomment-6282027387). Lost: a
  `Diagnostic` from `private_key`. A key file is not a document and has no span, the
  `Display` of a `Diagnostic` leaves out its code, and the caller loses the variant that
  `sdk-python` matches for its own fix of a passphrase (the same comment). The formats
  are a key choice of kind (f) that `laptop.architect-2` sent to `laptop.monitor` (plan
  review, 2026-10-19T23:24:32Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279566176). The
  profile is in `config`, beside `config::subject`, which takes the same key line; lost:
  a crate of its own for the profile, which must take `config::openssh` with it, to save
  only build time (the same comment). Lost: a `Pin` type for each `node` block. The map
  of address to key states that an address has one key, and the decoder refuses the
  second block, so a `Pin` adds a type with no rule of its own (`laptop.architect-2`,
  2026-10-20T01:11:29Z,
  https://github.com/synnaxlabs/foundation/pull/3472#issuecomment-6280967938).
