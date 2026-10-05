# R15: subject identity across forwarding nodes

Research fork 15, 2026-10-04. Scope: the BQ12 gap in r8 Q12 ("authenticate at hub,
authorize at the owner"). Inputs: the decision log (S8, S10, S11, C7, C8, D12, K4,
regions, BQ3, BQ6, BQ8, BQ10, read copies, transport shape), r5, r8, r13, r14.
Benchmark: `foundation-research/r15-sig/` (source, `results.txt`).

Marks: **[1]** one source only, **[U]** unverified or my estimate, **[M]** measured in
this fork. No mark: two or more sources, a locked decision, or code read in this repo.

---

## 0. Summary

**Recommendation.** A remote subject proves itself to every owner with its own
signature. The gateway carries the proof but cannot make one. There are exactly two
kinds of proof:

1. **Signed** (people, agents, programs, the CLI, every SDK). The SDK connects to any
   one node, the gateway. It signs a *hello*: subject, which of its keys, the gateway's
   node key, a random connection key, and an expiry (10 minutes, renewed at half). It
   also signs each session open and each control-plane request, as the exact bytes it
   sends. The gateway forwards the hello and the signed request to each owner. The
   owner checks both signatures against the subject's keys in the spec, checks that the
   hello names the peer node it sees, checks the expiry against mesh time, then
   authorizes with the access policy (C8). Frames on an open session carry no signature.
2. **Hosted** (connectors, calculations, the node itself). A node may act as subject X
   only when X is the node's own name, or the spec places connector X on that node (as
   its node or its standby). The owner checks it from the spec.

Node-to-node traffic (Raft, replica, clock exchange, blob fetch, relay) authenticates
node keys at the transport. The role the spec gives the peer authorizes it: voter,
current home, named standby or copy, time source. No access policy covers it.

**The trade.** A compromised gateway can still read and alter the traffic of subjects
connected through it. It can keep using their open sessions until their hello expires
(at most 10 minutes after they leave). It cannot act as a subject that did not connect
through it, open a session a subject did not ask for, raise a writer's authority, or
apply config. Direct connections to every owner (option 1) close that remaining gap,
but they put a second `hub` (routing, failover, carriers, NAT traversal, read copies)
into every hand-written SDK. Node trust (option 3) is what CockroachDB, Kafka's
controller forwarding, NATS routes, and the Synnax Core do inside one data center.
Across exposed edge nodes it makes any one node a key to the whole mesh.

**Cost [M].** Ed25519 verify takes 22.5 to 25 us and sign 5 to 10 us on an Apple M3
Max. On a Pi 4, verify takes about 267 us and sign about 87 us [1]. The cost is paid
per connection and per session open, never per frame.

**Revises r8 Q12.** "Authenticate at hub, authorize at the owner" becomes "the hub
admits the connection; the owner verifies the subject's own signature and authorizes".

---

## 1. The gap and the facts that frame it

A remote SDK on node A writes to an index homed on node H. H's transport authenticates
A, not the subject. If A only states the subject's name, H must trust A for every
subject, including subjects with `apply` and `admin`. The nodes most likely to be taken
over are the least protected: a Pi in a test cell, a DMZ gateway, a relay (r5).

Locked facts that constrain the answer:

- **C8:** subjects are names in the tree with keys, like nodes. Access is allow-only,
  default deny. Actions: read, write, plan, apply, secret, admin. A connector is
  vouched for by its node and may write under its own name.
- **S11:** the home's gate is the only control check. Authority is requested at writer
  open, capped by access. The control channel records the holder subject.
- **BQ3, C7, D12:** remote SDKs get the hub's surface over the network. Each SDK's data
  path is hand-written per language. No shared Rust core in SDKs.
- **Regions (K5):** a cut-off site keeps working. No check may need a node outside the
  site.
- **Read copies:** a weak link carries each sample once; the hub merges latest
  subscriptions for one remote home into one upstream flow.
- **Root CLAUDE.md:** one check per decision, no defense in depth.

---

## 2. Prior art

Each entry: the mechanism, its class (direct to owner, carried proof, node trust), and
the lesson for Foundation.

### 2.1 Tailscale (direct; destination enforces)

- Each device has its own key. The coordination server hands out public keys and
  policy. "Each node is responsible for blocking incoming connections that should not
  be allowed, at decryption time" (How Tailscale works). Rules are cached and enforced
  on each device, so they keep working when the coordination server is down
  (coordination-server-down KB).
- Forwarding: hosts behind a subnet router have no keys, so the router filters for
  them. Two bulletins were filtering gaps at such forwarding nodes: TS-2024-005 (Linux
  subnet routers and exit nodes accepted LAN connections into the tailnet) and
  TS-2025-006 (protocol filters not enforced on routers shared between tailnets) [1].
- Tailnet Lock: trusted keys sign each node key, and "Peer nodes can then verify the
  signature before allowing connections". A compromised coordination server cannot add
  nodes.
- Serve identity headers: the local `tailscaled` tells a backend who the user is. It
  strips such headers from incoming requests and advises that the backend listen only on
  localhost.

**Lesson.** Enforce at the destination. A node vouches only for what is local to it.
Peers can check the control plane's writes themselves.

### 2.2 NATS (proof at entry, node trust after)

- Chain: operator, account, user JWT. "The server verifies that signature against the
  account's identity key", and the client signs a server-issued nonce with its private
  key, which "proves the client holds the [private key] right now, not just a copy of
  the JWT". User JWTs can expire; revocation is an entry in the account JWT. Servers
  validate offline with the configured operator JWT.
- Inside a cluster, routes forward messages with no per-user proof. Route permissions
  only filter which subjects cross each route [1].
- Leaf nodes: "The leaf node authenticates and authorizes clients using a local
  policy." "Traffic between the leaf node and the cluster assumes the restrictions of
  the user configuration used to create the leaf connection." The hub never sees the
  leaf's users. This is scoped node trust.
- Service imports with `share` add requestor details in a header that the server
  writes, not the client [1].

**Lesson.** The nonce-signature login is the right SDK primitive, and it works offline.
NATS still trusts every server for what it forwards.

### 2.3 Kafka (direct for data, node trust for admin)

- "The producer sends data directly to the broker that is the leader for the partition
  without any intervening routing tier" (Confluent producer design). Every broker
  authenticates the client itself.
- KRaft admin forwarding wraps the client request in an Envelope. The controller "first
  authorizes the Envelope request using the authenticated broker principal. Then it
  authorizes the underlying request using the forwarded principal" (Kafka docs,
  KIP-590). Any broker may claim any principal.
- Delegation tokens (KIP-48): an HMAC under a master key that every broker holds. Any
  compromised broker can mint tokens.
- Client cost: the official Python client is "a lightweight wrapper around librdkafka"
  (PyPI). Direct-to-leader clients are heavy enough that Kafka's ecosystem shares one C
  core across languages.

**Lesson.** Direct-to-owner needs a heavy client per language. Kafka answered with one
C library; D12 rejected the same answer for Foundation.

### 2.4 CockroachDB (node trust in one cluster, scoped per tenant)

- "Whichever node receives the request acts as the 'gateway node'" (SQL layer docs).
  Physical plans run on other nodes.
- All nodes present `CN=node` certificates. `pkg/rpc/auth.go`: "we only allow RPCs if
  the client presents a valid root or node certificate". For tenant servers, KV nodes
  check the tenant ID in the certificate. SQL privileges are checked at the gateway; KV
  enforces only the tenant boundary [1].

**Lesson.** Node trust is the normal choice for nodes in one data center under one
operator. The tenant check shows the narrowing pattern: a forwarder may act only inside
the scope its own identity carries.

### 2.5 Synnax Core (node trust)

The API layer authorizes the writer at the gateway
(`core/pkg/api/framer/framer.go:475`). The distribution layer then opens peer writers
with a `ControlSubject` value and no proof
(`core/pkg/distribution/framer/writer/peer.go:60`, `service.go:76`). Same class as
CockroachDB.

### 2.6 Zenoh (hop-by-hop node trust)

ACL subjects match the directly connected instance by interface, certificate common
name, or username. "A router receives `put` messages and routes them, which applies both
of its matching `ingress` and `egress` rules". The spec: access control is "evaluated
locally by each node based on peer identity". A routed message carries no identity of
its first publisher [U: the docs are silent; inferred from the subject model].

### 2.7 DDS Security (direct; per-message MACs for origin)

- Each participant holds an identity certificate (Identity CA) and a permissions
  document (Permissions CA) whose `subject_name` must match the certificate. Grants list
  domains, topics, partitions, validity dates, and a default rule. Both travel in the
  authentication handshake, and each side checks the remote's permissions (OpenDDS
  guide, RTI manuals).
- Data flows peer to peer. To stop one reader forging a writer's messages on multicast,
  origin authentication appends receiver-specific MACs, so "a DataReader cannot
  impersonate the DataWriter" [1].
- RTI Routing Service by default "will change the metadata fields Writer GUID and
  Sequence Number in every sample"; `publish_with_original_info` copies the original
  values as plain metadata [1]. The router becomes the identity.

**Lesson.** Origin proof in a direct design costs a MAC per message. A forwarder
becomes the identity unless the original proof travels with the data.

### 2.8 Biscuit and macaroons (carried proof, offline narrowing)

- Biscuit: "a bearer token that supports offline attenuation, can be verified by any
  system that knows the root public key". Each attenuation appends a block signed by an
  ephemeral key; blocks cannot be removed. Datalog policies. Revocation IDs per block
  [1].
- Macaroons: chained HMAC with caveats; third-party caveats need discharge macaroons.
  All cryptography is symmetric, so the verifier needs the root secret. Fly.io keeps
  root keys in a separate service (`tkdb`) because "root secrets for Macaroon tokens are
  hazmat". Clients cache verifications, and Fly.io notes worries about verification over
  transoceanic links.

**Fit.** Macaroons would put the root secret on every owner, so any compromised owner
could mint tokens (node trust again), or would need a reachable verifier, which a
cut-off site lacks. Biscuit fits the cryptography, but it brings a second policy
language next to C8's selectors, against the simplicity directive. Foundation gets the
useful property (the holder narrows its own grant offline) by signing each request.

### 2.9 SPIFFE and SPIRE (scoped node trust for local workloads)

- The SPIRE server attests each node's agent. The agent attests local workloads by
  asking the OS about the calling process, then hands them SVIDs. The server sends an
  agent only "registration entries that have the agent's SPIFFE ID listed as their
  'parent SPIFFE ID'". A compromised agent can act only as workloads registered under
  it.
- JWT-SVIDs are bearer tokens: "Tokens sent to one audience can be replayed to another
  audience should more than one be present." The spec requires `aud` and `exp`.
- SPIFFE carries workload identity, not the end user through a call chain. The IETF
  Transaction Tokens draft (draft-ietf-oauth-transaction-tokens-11, July 2026) fills
  that gap with tokens short-lived "on the order of minutes or less", issued by exactly
  one Transaction Token Service per trust domain [1].

**Lesson.** SPIRE's parent ID is the hosted rule. Audience binding is required. A
central issuer needs reachability.

### 2.10 Kerberos delegation (carried proof, constrained by target only)

- Unconstrained delegation hands the service the user's TGT; a compromised service can
  act as that user against any service in the domain (Microsoft guidance, AD security
  writeups).
- Constrained delegation (S4U2Proxy) limits the target services. With protocol
  transition (S4U2Self), a compromised account "can pretend to be any user they want to
  the target service SPN" (SpecterOps). Resource-based constrained delegation lets the
  target choose who may delegate to it. The KDC must be reachable.

**Lesson.** Limiting where a forwarder may go does not limit whom it impersonates. Only
a proof that comes from the user does.

### 2.11 Google end-user permission tickets (carried proof, central issuer)

A central identity service checks the user's credential and returns a short-lived
ticket. "This ticket proves that the Gmail service is currently servicing a request on
behalf of that particular end user" (Google infrastructure whitepaper). Tickets are
forwarded down the call chain; each backend checks the ticket and the user's rights
(BeyondProd: tickets "reduce the need for trust between services"). MIT 6.5660 notes:
"RPC must be from approved service, user must actually be logged in".

**Lesson.** The property Foundation wants. The only difference is the issuer: Google
has a central service; a cut-off Foundation site has none, so the subject signs.

### 2.12 OPC UA gateways (industrial precedent)

An X509 user token is a signature over the `serverCertificate` and `serverNonce` of the
one server the client logs into. A Gateway Server "shall re-calculate the signatures on
the UserIdentityToken using the nonce provided by the underlying Server", and "shall use
its own user credentials if the UserIdentityToken provided by the Client does not
support impersonation" (OPC 10000-4, 5.7.3.1) [1]. A gateway can forward only tokens it
can read in full (passwords), which gives full impersonation. Key-based users fall back
to node trust.

**Lesson.** Binding a proof to each owner's nonce blocks forwarding. Foundation binds
the proof to the gateway (the audience) and limits it in time and to the requests the
subject signed.

### 2.13 Vocabulary and sender binding

RFC 8693 separates impersonation (P is "indistinguishable from B") from delegation (P
keeps its own identity, recorded in the `act` claim) [1]. Foundation records both the
subject and the forwarding node. RFC 8705 (certificate-bound tokens) and RFC 9449 (DPoP)
bind a token to the holder's key so a copied token is useless elsewhere; the hello's
audience field does the same job.

### 2.14 Summary of prior art

| System | Class | A compromised forwarder can |
|---|---|---|
| Tailscale | direct, destination enforces | nothing beyond hosts behind it |
| NATS cluster, leaf | proof at entry, node trust after | publish as any user it serves (leaf: within the leaf user's rights) |
| Kafka data | direct | n/a |
| Kafka admin forwarding | node trust | send admin requests as any principal |
| CockroachDB | node trust, tenant-scoped | act as any SQL user (KV node: any tenant) |
| Synnax Core | node trust | write as any subject |
| Zenoh | hop-by-hop node trust | publish anything its own identity allows |
| DDS | direct, MACs per message | n/a (Routing Service: becomes the writer) |
| SPIRE | scoped node trust | act as workloads registered under it |
| Kerberos constrained + protocol transition | node trust to listed targets | act as any user to those targets |
| Google tickets, Txn-Tokens | carried proof, central issuer | act only for users it is serving now, until ticket expiry |
| OPC UA gateway | password forwarding or node trust | full impersonation, or anything its own login allows |

Systems inside one data center under one operator use node trust. Systems spanning
untrusted or exposed nodes connect directly or carry a proof from the user.
Foundation's edge nodes sit in test cells and DMZs, and its sites must work cut off, so
it needs a carried proof without a central issuer.

---

## 3. Options compared

- **Option 1, direct.** The SDK connects to every owner with its own key, through
  relays when needed (relays see ciphertext only, r5). Any node answers metadata
  (homes, copies), as in Kafka.
- **Option 2a, delegation per connection.** One gateway connection. The subject signs
  "node A may act for me until E"; A presents it at any owner for any session.
- **Option 2b, signed requests (recommended).** As 2a, plus the subject signs each
  session open and control-plane request; owners accept only what was signed.
- **Option 3a, node trust.** A node vouches for any subject it forwards.
- **Option 3b, scoped node trust.** A node vouches only for subjects listed for it
  (Kerberos constrained delegation, NATS leaf users, SPIRE parent IDs).

All options concede the same base: a compromised node controls the indexes it homes,
the connectors it runs, the copies it holds, and anything it relays (drop or delay).
Placement is the trust decision for those.

| Criterion | 1 Direct | 2a Delegation | 2b Signed requests | 3a Node trust | 3b Scoped |
|---|---|---|---|---|---|
| Compromised node, beyond the base | nothing | act as each subject connected through it, with all that subject's rights, until hello expiry | only the sessions those subjects opened, until hello expiry | act as any subject, including `apply` and `admin`: whole-mesh takeover | act as each listed subject at any time, connected or not |
| Reads and alters traffic of its clients | no (end-to-end encryption) | yes | yes | yes | yes |
| Revoke a subject | apply removes key or grant; owners close sessions | same | same | same | same |
| Revoke a node | n/a | removing its key voids all its hellos | same | removing its key; until then, unlimited | same as 3a for its list |
| Window after the subject leaves | 0 | hello lifetime | hello lifetime | unlimited | unlimited |
| Session open | dial each owner: 1 RTT (QUIC) to 2 RTT (TLS over TCP), more via relay | gateway already connected; gateway to owner open | same as 2a, plus one sign per open | same as 2a | same as 2a |
| Steady-state hops | one | gateway hop plus decrypt and re-encrypt, unless the gateway is the owner | same | same | same |
| Crypto per frame | none (transport only) | none | none | none | none |
| Crypto per session | sign per owner connection | sign per hello; verify per hello per owner | plus sign per open; verify per open per owner | none | none |
| SDK work per language | carriers, NAT traversal, routing from mesh runtime, failover follow, re-index history, live selector expansion across homes, read copies: a second hub | Ed25519 sign of one small message | plus sign the bytes of each open | login only | login only |
| Cut-off site | works if the SDK reaches owners | works (keys and mesh time are local) | works | works | works |
| Simplicity | new SDK machinery, routing in N languages | one internal message | two internal messages | no new parts | new list of who vouches for whom |
| Precedent | Tailscale, Kafka data, DDS | Kafka delegation tokens, JWT-SVID | Google tickets, Txn-Tokens (with an issuer) | CockroachDB, Kafka admin, Synnax | NATS leaf, SPIRE, Kerberos KCD |

Notes on the deciding criteria:

- **SDK cost decides against option 1.** D12 makes every SDK's data path hand-written.
  Option 1 adds the hub's hardest parts to each language. It also bypasses the hub's
  merged upstream flows and read copies, so a weak link carries each sample once per
  remote SDK again (r14 weakest point 2).
- **Latency (P1).** The P1 target is one LAN hop. Options 2 and 3 meet it when the
  program connects to the node that homes its channels; then the gateway is the owner
  and no forward happens. A program that spans homes on several nodes pays one more hop
  and one more AEAD pass per byte at the gateway. Option 1 avoids both, at the SDK cost
  above.
- **Option 3 fails the threat.** Under 3a one compromised Pi can run `apply` as an
  admin. 3b narrows the list but still lets a node act for listed subjects while they
  are away, and it needs new config.
- **2b over 2a.** Under 2a, an operator who connects through a compromised gateway
  only to watch a dashboard lends that gateway write access at authority 250 to every
  valve the operator may control. Under 2b the gateway can only reuse the read sessions
  the operator opened. The extra cost is one signature per session open over bytes the
  SDK already produces.

---

## 4. Recommended model

### 4.1 Rules

1. Every data-plane session and control-plane request at an owner names one subject
   and carries one proof: *signed* or *hosted*. Nothing else is accepted.
2. **Signed proof.** The hello is valid while `now.latest < expires` and
   `expires <= now.earliest + cap` on the owner's mesh clock (C6 interval). It must name
   the peer node the owner's transport authenticated. The signed open must use the same
   key and the same connection key. Both signatures verify against a key the spec lists
   for the subject.
3. **Hosted proof.** The peer node may act as X only when X is the node's own name, or
   the spec places connector X on that node (its `node` or its placement standby).
   Calculations are connectors (C5), so the rule covers them.
4. A signed open is valid for the life of its connection. Reopening the same session
   key at the same or a new owner (reconnect, failover, a new home for a live
   selector) replaces the earlier attachment. No replay cache is needed: the gateway can
   only repeat what the subject asked for.
5. The SDK renews the hello at half its life. The gateway forwards each renewal to every
   owner that holds sessions of the connection. An owner closes the connection's
   sessions when the hello expires.
6. The gateway's hub verifies the hello once to *admit* the connection (it will not
   spend resources on an unknown peer). Each owner verifies to *authorize* its own
   sessions. Different decisions, so not a second guard. When the gateway is the owner,
   the hello is verified once and cached for the connection.
7. Authorization stays where Q12 put it: home for read and write (with the S11 gate),
   the region's voters for plan, apply, secret, admin, and the executing node for
   operations such as `discover`.
8. Authority comes from the signed open, capped by access (S11). The gateway cannot
   raise it.
9. A program on the same machine as its node uses the same signed path over loopback.
   No OS-level process identity (SPIRE needs a different attestor per OS for that).

### 4.2 Sketch

```rust
// Signed by one of the subject's keys. Sent first on every SDK connection.
pub struct Hello {
    subject: Name,         // people.alice
    key: PublicKey,        // must be listed for the subject in the spec
    via: node::Key,        // audience: the gateway node
    connection: [u8; 16],  // random; names this connection at every owner
    nonce: [u8; 16],       // from the gateway; only the gateway checks it
    expires: time::Stamp,  // mesh time
}

// A session open or control-plane request: the exact bytes the SDK sent.
pub struct Signed {
    connection: [u8; 16],
    body: Block,
    signature: [u8; 64],
}

pub enum Proof {
    Hosted,                                  // peer runs the subject (rule 3)
    Signed { hello: Hello, hello_sig: [u8; 64], request: Signed },
}

// Layer 1, pure. Called by hub (admission), home, mesh, and executing nodes.
pub fn admit(spec: &spec::Tree, now: Interval, peer: node::Key, subject: &Name,
             proof: &Proof) -> Result<(), Denied>;
```

The SDK takes `expires` from the mesh time the gateway reports at connect, advanced by
its own monotonic clock. A lying gateway cannot stretch the hello past `cap` (rule 2).

### 4.3 Flows

**(a) Remote SDK writes a command through gateway A to home H.**

1. The Python SDK opens TLS to A (pinned node key), receives a nonce and mesh time, and
   sends a signed hello.
2. A's hub checks the hello and admits the connection.
3. The SDK sends a signed writer open (channels, authority 200, lease).
4. A routes the open to H with the hello and the signed bytes, over A's node-key
   connection.
5. H runs `access::admit` (two verifies, about 50 us on an M3, about 0.5 ms on a Pi 4,
   then cached per connection for the hello), then the C8 policy and the S11 gate.
6. Frames flow on the session with no signatures. The control channel records
   `people.alice via site_a.gateway_1` at authority 200 (decision 7).

**(b) In-process connector on A writes to H.** The connector calls `hub::writer` with
subject `site_a.plc_7`. A opens the session at H with `Proof::Hosted`. H checks that the
spec places `site_a.plc_7` on A (or on A as its standby). No signature.

**(c) Apply through a gateway.** The CLI signs an apply request whose body holds the
plan hash and the base spec version. The gateway forwards it to the region's voters.
The voters verify the signature and check `apply` against the base spec. The
`mesh.changes` record keeps the signed bytes, so every node can check it (decision 5).

**(d) Replica stream from home H to standby S.** No subject. S's transport authenticates
H's node key. S accepts the stream only when the runtime record names H as the current
home of the index, under the current fence epoch, and the placement names S as standby
or copy (BQ6, BQ8).

### 4.4 Internal traffic

| Traffic | Who may send | Authorized by |
|---|---|---|
| Transport handshake | a member node key, or a joining node with a signed ticket | runtime membership (Q11), spec node keys (S8) |
| Relay | forwards only between member keys; sees ciphertext | r5 relay rule |
| Raft per region | the region's voters | the `region` block's `voters` |
| `mesh.changes`, spec blobs | any member reads; content checked by hash from the Raft pointer | S9, BQ1 `blob` |
| Replica | current home (fence epoch) to its placement standby or copies | placement, runtime homes |
| Clock exchange | any member may ask; a node uses only the sources its time policy names | C6 |
| Node status (`<node>.*`) | the node itself, hosted rule | C8 own-name rule |
| Upgrade binaries | any member serves; release signature checked | C9d |

All of it comes from config users already write (regions, placement, time policy). An
access policy that could break replication by mistake does not exist.

### 4.5 What a compromised node can do under the model

| Role of the node | Reach |
|---|---|
| Any member | act as itself and the connectors placed on it; read and alter the traffic of subjects connected through it; reuse their open sessions until hello expiry; drop or delay what it relays |
| Home of an index | write any data into the index, run its gate, lie to its readers (the home is the authority; placement is the trust decision) |
| Standby or copy | read the copied data; take over only by the voters' lease decision |
| Voter | vote and stall; with decision 5, cannot forge spec changes or move homes outside placement |
| Relay | drop or delay; ciphertext only |
| Time source | shift the clocks that follow it, within what the estimator accepts |

### 4.6 Costs

Measured on an Apple M3 Max, 128-byte message, three runs [M]:

| Library | Sign | Verify |
|---|---|---|
| ed25519-dalek 2 | 10.1 to 10.4 us | 24.7 to 24.8 us (`verify_strict` 27.3 to 28.0 us) |
| aws-lc-rs 1 (r7's chosen provider) | 5.2 to 5.3 us | 22.5 to 25.0 us |

Pi 4 (Cortex-A72, 1.5 GHz, eBACS best implementation) [1]: sign 130,552 cycles (about
87 us), verify 400,122 cycles (about 267 us).

- Per frame: zero. Transport encryption only.
- Per connection: one sign at the SDK, one verify at the gateway and one at each owner,
  every 5 minutes.
- Per session open: one sign at the SDK, one verify at each owner.
- Worst case: 1,000 remote sessions reopening at a Pi 4 standby after failover cost
  about 0.27 s of one core [U: arithmetic from eBACS]. `verify_batch` cuts it further.
- Per byte at a gateway that is not the owner: one AEAD decrypt and encrypt. Bulk
  writers and 1 kHz loops should connect to the home's node.

SDK per language:

- Python: Ed25519 needs the `cryptography` package (not in the standard library).
  Python 3.14's `ssl` offers only `tls-unique` channel binding [M], which RFC 9266 says
  is not defined for TLS 1.3. So the hello uses the gateway nonce and the audience
  instead of TLS channel binding. The SDK pins the gateway's node key on its TLS
  certificate.
- Rust: reuses the `access` message encoding and `aws-lc-rs`.
- A later TypeScript SDK in a browser: Web Crypto has Ed25519 in Firefox 129, Safari
  17, and Chromium 137 [1], with non-extractable keys.

### 4.7 Seating

- `access` (layer 1, pure) owns `Hello`, `Signed`, `Proof`, and `admit`, beside the
  policy check. One crate decides "may this subject do this", including "is this really
  the subject".
- `hub` admits SDK connections, forwards proofs with each open, and forwards renewals.
  It never authorizes.
- `home`, `mesh` (voters), and nodes executing operations call `access::admit`, then the
  policy check.
- `transport` stays as r5 locked it: node keys only, never names or subjects.
- Internal protocols (`raft`, `replica`, `clock`, `blob`) check roles from the spec and
  runtime they already read. No new dependency.

Nothing reaches into another crate's internals. The feature sits on public surfaces:
`access` is pure, and every caller already holds the spec, the peer key, and mesh time.

---

## 5. Changes to locked decisions

- **r8 Q12:** revised as in section 0.
- **C8:** "a connector is vouched for by its node" stays, limited to placement (rule 3).
  Subjects get a list of keys (decision 3).
- **S11:** the control channel value adds the forwarding node (decision 7).
- **BQ3:** surface unchanged; remote SDKs add the hello and signed opens.
- **K4:** the caller seals secret values (decision 9).
- **C7:** MCP runs beside the agent (decision 8).

---

## 6. Decisions for the user

1. **Two proofs only.** Remote subjects prove themselves with their own signature
   carried by the gateway. Connectors, calculations, and nodes are vouched for by the
   node the spec places them on. *Recommend: adopt.* Option 1 duplicates the hub in
   every SDK; option 3 makes one compromised edge node a key to the mesh.
2. **Sign each session open (2b), not one delegation per connection (2a).**
   *Recommend 2b.* A compromised gateway can reuse only the sessions a subject opened,
   at the authority it asked for. Cost: one signature per open over bytes the SDK
   already sends.
3. **Subject keys.** Each subject lists one or more Ed25519 public keys (one per device
   or CI job), so one device is revoked by removing its key. Accept OpenSSH
   `ssh-ed25519` text, and let SDKs sign through `ssh-agent` (Git's SSH signing
   precedent). Later: hardware keys and SSO as a short-lived certificate from an issuer
   key that a region trusts, checked by the same `admit`. *Recommend: key list and
   OpenSSH format now; issuer certificates later.*
4. **Control-plane requests are signed over the exact body.** Apply signs the plan hash
   and base spec version, so `apply` commits exactly the reviewed plan (K3) even through
   a compromised gateway. `mesh.changes` keeps the signed bytes. *Recommend: yes.*
5. **Every node checks every `mesh.changes` record.** Spec changes: subject signature
   and permission against the base spec. Home moves: only to a placement standby. Joins:
   only with a signed ticket. Same idea as Tailnet Lock. A compromised voter can stall a
   region but cannot rewrite access, keys, or placement. Cost: one verify per change.
   *Recommend: yes.*
6. **Internal traffic is authorized by role, never by access policies.** Roles come
   from regions, placement, and time policy. *Recommend: yes.* An access policy for
   Raft or replication would be a new way to break the mesh by mistake.
7. **Audit records the subject and the forwarding node** (RFC 8693 delegation, not
   impersonation). The control channel value becomes holder subject, via node, and
   authority. Owners report denials with both. *Recommend: yes.* It changes S11's value
   shape.
8. **The MCP server runs as a local process beside the agent**, holding the agent
   subject's key, over stdio. No MCP hosted inside nodes in v1: a hosted one would need
   node trust for agents. *Recommend: yes.*
9. **The caller seals secret values** to the target nodes' keys before sending. Gateways
   and voters see ciphertext only; the request is signed with the `secret` action.
   *Recommend: yes.* It settles who seals in K4 and r8 Q16.
10. **No end-to-end integrity on frames in v1.** The hub splits frames by home, so an
    SDK-side MAC per frame would make the SDK know homes (option 1's cost). Programs
    that cannot accept a gateway in the path connect to the node that homes their
    channels. *Recommend: accept the trade; revisit only if a user needs untrusted
    gateways.*

Parameters, tuned later and not interviewed: hello lifetime 10 minutes, renewal at half,
owner cap 15 minutes.

---

## 7. Sources

Tailscale:
- How Tailscale works: https://tailscale.com/blog/how-tailscale-works
- Coordination server down: https://tailscale.com/kb/1091/what-happens-if-the-coordination-server-is-down
- Security bulletins (TS-2024-005, TS-2025-006): https://tailscale.com/security-bulletins
- Tailnet Lock: https://tailscale.com/kb/1226/tailnet-lock
- Serve identity headers: https://tailscale.com/kb/1312/serve

NATS:
- NKey auth: https://docs.nats.io/running-a-nats-service/configuration/securing_nats/auth_intro/nkey_auth
- JWT auth: https://docs.nats.io/running-a-nats-service/configuration/securing_nats/jwt
- Leaf nodes: https://github.com/nats-io/nats.docs/blob/master/running-a-nats-service/configuration/leafnodes/README.md
- Route permissions: https://docs.nats.io/reference/config/cluster/permissions.md
- Service import `share`: https://docs.nats.io/using-nats/nats-tools/nsc/services

Kafka:
- Producer design: https://docs.confluent.io/kafka/design/producer-design.html
- Authorization and Envelope forwarding: https://kafka.apache.org/39/security/authorization-and-acls/
- KIP-590: https://cwiki.apache.org/confluence/display/KAFKA/KIP-590%3A+Redirect+Zookeeper+Mutation+Protocols+to+The+Controller
- KIP-48 delegation tokens: https://cwiki.apache.org/confluence/display/KAFKA/KIP-48+Delegation+token+support+for+Kafka
- confluent-kafka (Python): https://pypi.org/project/confluent-kafka/

CockroachDB:
- SQL layer: https://docs.cockroachlabs.com/docs/stable/architecture/sql-layer
- Authentication: https://docs.cockroachlabs.com/docs/v25.4/authentication
- RPC authorizer: https://github.com/cockroachdb/cockroach/blob/master/pkg/rpc/auth.go

Zenoh:
- Access control manual: https://zenoh.io/docs/manual/access-control/
- Spec, access control: https://spec.zenoh.io/spec/1.0.0/security/access-control.html

DDS:
- OpenDDS security guide: https://opendds.readthedocs.io/en/latest-release/devguide/dds_security.html
- RTI Connext Micro security SDK (origin authentication): https://community.rti.com/static/documentation/connext-micro/3.0.3/doc/html/usersmanual/security_sdk.html
- RTI Routing Service original info: https://community.rti.com/node/3567

Tokens:
- Biscuit specification: https://doc.biscuitsec.org/reference/specifications
- Macaroons Escalated Quickly: https://fly.io/blog/macaroons-escalated-quickly/
- Operationalizing Macaroons: https://fly.io/blog/operationalizing-macaroons/
- Transaction Tokens draft 11: https://datatracker.ietf.org/doc/html/draft-ietf-oauth-transaction-tokens
- RFC 8693: https://rfc-editor.org/rfc/rfc8693
- RFC 9266: https://rfc-editor.org/rfc/rfc9266.html
- RFC 9449 and RFC 8705 overview: https://workos.com/blog/mtls-dpop-token-binding-sender-constrained-oauth

SPIFFE:
- SPIRE concepts: https://spiffe.io/docs/latest/spire-about/spire-concepts/
- JWT-SVID: https://spiffe.io/docs/latest/spiffe-specs/jwt-svid/
- Compromised-node blast radius (secondary): https://skycloak.io/blog/spiffe-spire-node-compromise-workload-identity/

Kerberos:
- S4U2Pwnage: https://specterops.io/blog/2017/01/05/s4u2pwnage/
- FreeIPA constrained delegation design: https://freeipa.readthedocs.io/en/stable/_sources/designs/rbcd.md
- Unconstrained delegation risk: https://adsecurity.org/?p=4056

Google:
- BeyondProd: https://docs.cloud.google.com/docs/security/beyondprod
- Infrastructure security design overview (PDF): https://css.csail.mit.edu/6.566/2018/readings/google-infrastructure.pdf
- MIT 6.5660 lecture notes: https://css.csail.mit.edu/6.5660/2024/lec/l07-google.txt

OPC UA:
- OPC 10000-4, 5.7.3.1 ActivateSession: https://reference.opcfoundation.org/specs/OPC-10000-4/5.7.3.1

Performance and platforms:
- eBACS, Pi 4 signatures: https://bench.cr.yp.to/results-sign/aarch64-pi4b.html
- Ed25519 in browsers: https://blog.ipfs.tech/2025-08-ed25519/
- This fork: `foundation-research/r15-sig/` (Rust source, `results.txt`)

Repo:
- `core/pkg/api/framer/framer.go:475`, `core/pkg/distribution/framer/writer/peer.go:60`,
  `core/pkg/distribution/framer/writer/service.go:76`

---

## 8. Single-source and unverified claims

- [1] Tailscale bulletin details; NATS route behavior and `share` header; CockroachDB's
  split (SQL privileges at the gateway, tenant only at KV); DDS origin authentication
  and Routing Service metadata (RTI only); Biscuit properties (spec only); the
  Transaction Tokens lifetime; the OPC UA gateway rule (spec only); RFC 8693 wording;
  browser Ed25519 versions; Pi 4 Ed25519 cycles (eBACS only).
- [U] Zenoh carries no first-publisher identity on routed messages (docs silent); the
  failover-storm arithmetic; Python SDK pinning a node key through the standard `ssl`
  module (not tried).
- [M] M3 Max signature timings; Python 3.14's channel binding types.
