- **PROFILE (2026-10-20)** The profile is the HCL file of a program's host that names
  its subject and pins the public key of each node that it connects to. `sdk-python`
  reads it at `FOUNDATION_PROFILE`, else `~/.foundation/profile.hcl` (SDK PYTHON in
  `docs/decisions/operations/sdk-python.md`). It has three optional attributes and
  one block kind:

  ```hcl
  subject     = "plant.alice"          # a subject name
  private_key = "alice.key"            # relative to the profile's directory
  strict      = true                   # refuse a node that no source pins

  node "10.0.0.5:7400" {
    public_key = "ssh-ed25519 AAAA... node@host"
  }
  ```

  `config::profile::decode(&Document)` gives `config::profile::Profile`: the
  subject, the key file path, `strict` (default `false`), and a map from each
  address (`ip:port`) to its Ed25519 public key. A `node` block holds only the public
  key (`laptop.architect-2`, 2026-10-19T23:49:24Z, item 1 of
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279895005). Each
  problem is a `Diagnostic` with its fix: an unknown attribute or block, a wrong type,
  a bad name, an empty path, a `node` label that is not `ip:port`, a key line that is
  not `ssh-ed25519`, and a second block of one address, with a note at the first.
  `config::profile::node(address, public_key)` gives the `node` block of a pin as a
  `document::Block`, and the program writes the file with `config_hcl::update`, so
  each other byte of the file stays (`laptop.architect-2`, item 2 of the same
  comment). The key file is the OpenSSH private key file of an Ed25519 key with no
  passphrase, as `ssh-keygen -t ed25519 -N ""` writes it. `config::openssh` reads and
  writes it with `ssh-key`, and refuses another algorithm by its name (an ECDSA key
  as `ECDSA`, which `ssh-key` reads only with its `ecdsa` feature), an encrypted key,
  and a text that is not a key file. The formats are a key choice of kind (f) that
  `laptop.architect-2` sent to `laptop.monitor` (plan review, 2026-10-19T23:24:32Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279566176). The
  profile is in `config`, beside `config::subject`, which reads the same key line;
  lost: a crate of its own for the profile, which must take `config::openssh` with
  it, to save only build time (the same comment). Lost: a `Pin` type for each
  `node` block. The map of address to key states that an address has one key, and
  the decoder refuses the second block, so a `Pin` adds a type with no rule of its
  own.

  `foundation profile [--data <dir>] [--json] <address>` prints the `node` block that
  pins the node of `<dir>` (default `foundation-data`) at `<address>`, for the profile
  of a program on another host. The operator runs it on the node's host (item 4 of the
  plan review, 2026-10-19T23:24:32Z,
  https://github.com/synnaxlabs/foundation/issues/3212#issuecomment-6279566176). The
  node must have started once: `node::public_key` reads the public key from the file
  `node.key`, so a node that runs, that stopped, or that crashed gives its key. It
  writes nothing and needs no lock. A data directory with no `node.key`
  (`node::Error::Unstarted`) gives `node.unstarted` with the fix "Start the node once
  with `foundation start --data=<dir>`, with `--name <name>`", a file that this user
  cannot read gives `node.unreadable`, and a `node.key` that a node did not write gives
  `node.failed`. `<address>` is where the program dials the node: an unspecified IP or
  port 0 gives `ops.argument`. The output is the block as HCL, which
  `config_hcl::write` gives, or with `--json` one line `{"address": "<ip:port>",
  "public_key": "<the line of an OpenSSH .pub file>"}`, a format of kind (f).
  `ops::cli` writes either, and each error, in the one output form of OPS OUTPUT, with
  the key from its `public_key` input, which `node` gives as a read of
  `node::public_key` (NODE START). `profile` calls `public_key` once and never calls
  `reach`. It is not an MCP tool, as `start` is not. `ops` takes `config-hcl` as a
  normal dependency for it.
  `laptop.architect-2` gave this shape in
  https://github.com/synnaxlabs/foundation/pull/3479#issuecomment-6281210000
  (2026-10-20T01:31:13Z) and
  https://github.com/synnaxlabs/foundation/pull/3479#issuecomment-6281253212
  (2026-10-20T01:34:49Z). Lost: a read of the file `address` through
  `sector::local::read`, which refuses a node that stopped; the `hcl` front end of
  `FrontEnds` as the writer, which keeps the format of a file that `ops` writes in
  `node`'s table; and a `Run::Profile` that `node` writes, which gives a second output
  form.
