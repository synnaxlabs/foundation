- **NODE KEY TLS** Every carrier but the diode runs TLS 1.3 only. A node's certificate
  is self-signed from a fixed template: Ed25519 key, `CN=foundation`, serial 1, valid
  from 1970 to `99991231235959Z`. The same key always gives the same bytes. A peer is
  the Ed25519 key in the leaf certificate's `SubjectPublicKeyInfo`; names, dates, and
  issuer are not checked. A peer's chain is one certificate of at most 1 KiB; any other
  chain is refused, so a peer cannot make the node hold more for a session (#299). The
  limit is part of `foundation/1`: a certificate over it needs a new ALPN. The person
  approved it on 2026-10-05 ("approve"), #383. A node sends its certificate when it
  dials; an SDK client sends none and pins the node key the same way. ALPN is
  `foundation/1`, and a new session protocol gets a new name. During an upgrade, a node
  accepts its own ALPN name and the previous one, and offers its own name only after
  every node runs the release (C9d). A session that agrees no ALPN, or a name the node
  does not accept, ends on every carrier. The suites are AES-128-GCM, AES-256-GCM, and
  ChaCha20-Poly1305; the groups are X25519MLKEM768, X25519, P-256, and P-384. A dialing
  node offers them in that order, and the client's order decides, so nodes agree
  AES-128-GCM and X25519MLKEM768. The person chose "AES-128-GCM" first between nodes and
  "Hybrid first" on 2026-10-05. A node accepts any one suite and group, so an SDK may
  offer only one. Resumption and 0-RTT are off, so rustls gets a fixed time and never
  reads the OS clock. Randomness inside TLS comes from aws-lc (TLS RANDOMNESS). Decided
  by `network` in #54; the ALPN check, suites, and groups in #108. The person accepted
  it as a contract on 2026-10-05 ("yes to both"). The golden certificate, the ALPN name,
  and the suite and group lists are an oracle in `oracles/conformance/transport/`. A key
  of small order is not a node key: a signature for it passes with no private key, so
  every Ed25519 check refuses it (BQ12). `types::ed25519::PublicKey` refuses such a
  key when it is built, so no check site needs its own test. The person decided on
  2026-10-05 ("Yeah that's fine"), #227, #277. `types::ed25519::PublicKey` holds the
  Ed25519 public key of a node and of a subject. Decided by `laptop.architect` at
  2026-10-08T04:03:21Z
  (https://github.com/synnaxlabs/foundation/issues/1755#issuecomment-6051941741).
  `types::ed25519::PrivateKey` holds the Ed25519 private key of a node and of a
  subject (ruling above). It moved in its own mechanical PR before the PR that gives
  `hub::client` the private key of a subject. Ordered by `laptop.director` at
  2026-10-08T05:41:28Z
  (https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6053189498).
  `types::ed25519::Pair::new` is the one place that derives the public key from the
  private key (`Pair` ruling below). `PrivateKey::public` derives through it, for a
  caller that needs only the key, or needs it once at an open or a join: `transport`
  and `mesh` at open, `mesh::Ticket`, and tests. A holder that signs for each message
  or request keeps one `Pair`. No crate keeps a copy. So
  `types` depends on `aws-lc-rs`, as it owns the Ed25519 rule of the key. Cost: each
  crate that depends on `types` builds `aws-lc-rs` one time for each target directory.
  Lost: a `pub fn` in `transport`, a pass-through for a thing that is not transport;
  and the copies, which grow with each crate that needs the key. Decided by
  `laptop.architect` (2026-10-07T14:16:15Z):
  https://github.com/synnaxlabs/foundation/issues/1423#issuecomment-6039878050. The
  first sentence was changed by `laptop.architect` at 2026-10-08T09:12:15Z
  (https://github.com/synnaxlabs/foundation/pull/1843#issuecomment-6056619178).
  `types::ed25519::PublicKey::verify` is the one Ed25519 verify, and gives
  `BadSignature` for a signature that is not of the message by the key. A verify on
  `PublicKey` uses a key that is not of small order by construction. `mesh` and
  `access` call it. Lost: a `bool`, which a caller can invert or drop
  with no word from the compiler; a `Signature` type, as `[u8; 64]` already fixes the
  length; and a copy in `access`. Decided by `laptop.architect` at 2026-10-08T05:45:46Z
  (https://github.com/synnaxlabs/foundation/issues/1747#issuecomment-6053244858). The
  TLS CertificateVerify is the exception: rustls checks it with the Ed25519 of
  `aws-lc-rs`, as a step of the TLS 1.3 handshake, and `transport` takes the
  certificate's key only as a `PublicKey`, so a key of small order ends the handshake.
  If `PublicKey::verify` gets a check that `aws-lc-rs` does not make, both TLS verifiers
  call it. Lost: a call of `PublicKey::verify` in each TLS verifier, which moves the
  check of the scheme and of the signature out of rustls, a mature library that makes
  them, and adds no check that the handshake does not make. Decided by
  `laptop.architect` at 2026-10-08T08:04:32Z
  (https://github.com/synnaxlabs/foundation/pull/1812#issuecomment-6055539099).
  `types::ed25519::Pair` is the one Ed25519 sign. Each signer (`mesh::claim::Signer`,
  `mesh::card::Signed::sign`, `mesh::Ticket::admission`, `transport::Tls::new`, and
  `hub::client` in #1748) builds one `Pair` and keeps no other signing key. The TLS
  CertificateVerify is the exception: rustls signs it with the key of the PKCS#8
  document that `transport` builds, as a step of the TLS 1.3 handshake. Lost: a
  `PrivateKey` that owns the pair, which re-derives on each clone and changes each
  constructor; and a `PrivateKey::sign`, which costs a scalar multiplication for each
  claim and each request of `hub::client`. Decided by `laptop.architect` at
  2026-10-08T08:12:49Z
  (https://github.com/synnaxlabs/foundation/issues/1748#issuecomment-6055665349).
  `transport::fuzzing::peer(chain)` and `transport::fuzzing::certificate`, behind the
  `fuzzing` feature, give the fuzz target `transport_certificate` the server's
  reading of a dialer's chain and a node's certificate. The protocol is fixed at
  `foundation/1`: rustls agrees only a protocol from the server's own list, so the
  compare sees that protocol or none, which the handshake tests cover. Approved by
  `laptop.architect-2` at 2026-10-08T15:11:07Z
  (https://github.com/synnaxlabs/foundation/pull/1899#issuecomment-6062925892). The
  reason was changed by `laptop.architect-2` at 2026-10-08T15:31:46Z
  (https://github.com/synnaxlabs/foundation/pull/1899#issuecomment-6063367399).
