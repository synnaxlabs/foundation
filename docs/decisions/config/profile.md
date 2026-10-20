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
