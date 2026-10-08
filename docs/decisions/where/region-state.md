# Runtime agreed state (region state)


| Concept | Defined or stored | Written by | Read by | Owner crate |
| --- | --- | --- | --- | --- |
| Node | Region state: membership record `{ key, card { name, public key, seal key, addresses, version } signed by the node, admission, ephemeral, status keys by name }` (MEMBER RECORD) in the region that holds the node's name. Private key: node-local. Files only name nodes | Voters at join (ticket); removal operation; removal of an ephemeral node after its time offline | `mesh`, `hub` (authentication), `access`, `plan` (name checks) | `mesh` (record), `node` (key material) |
| Membership | Region state: node records plus each region's voter set | Voters | Everyone | `mesh` |
| Node lease | Region state of the node's own region | The node renews; a renewal carries its version and seq block requests | Voters (promotion), `home` (fence, with the clock bound) | `mesh`, `home` |
| Actual home of an index | Region state of the home node's region: `{ home node, holder, seq block }` | Voters (promotion), `apply` (the first home of an index that a spec change adds) | `hub` routing through `mesh` watches | `mesh` |
| Seq blocks | Region state of the home node's region | The home, through lease renewals | A new home after promotion | `mesh` |
| Index history (re-index) | Region state: spans and seals. The spec keeps only the current index. Which region: X39 | The old home proposes the seal; voters seal at lease end if it is down | `hub` joins spans for readers | `mesh` |
| Secret ciphertexts | Region state, outside the spec, one per eligible node (region of the secret: X40), with a version per name in the associated data. Every node takes a write or a delete only at the newest version plus one, and a re-seal only at the newest version, from and to nodes of the secret's placement. A delete is a version with no value. The newest version of a name is never compacted away, also after the spec removes the secret | `secret set` and `secret delete` (`ops` calls `secret::seal`) | The node that runs the connector opens it in `secret::store::Sealed`, which refuses a value that does not open at its version | `mesh` (record), `secret` (seal and open) |
| Join ticket record | Region state: options and use count. The ticket itself is a secret, never in files | Admin through `ops` | Voters at join | `mesh`, `ops` |
| Delegation record | The parent region's spec: `{ prefix, epoch, initial voters }` | Parent voters | Nodes (epoch fencing) | `mesh` |
| Spec pointer | Region state: `{ version, root hash }` | `apply` (compare-and-swap) | Every node that follows the region | `spec` (type), `mesh` (record, compare-and-swap) |
| Spec tree | Prolly tree chunks in the `blob` store on each node's disk | `apply` writes chunks | Nodes fetch the ranges they use | `spec` (tree), `blob` (chunks) |
| Changes channel | The region's Raft log presented as a channel; seq is the log index; one per region (X29) | Voters | Any node, `plan`, agents | `mesh` (served through `hub`) |
| Desired version, rollout lock, format flag | Desired version in the spec; lock and flag in region state (multi-region scope: `docs/decisions/open/still-open.md`) | `ops upgrade`; voters | `node` (binary swap); `codec`, `wire`, `buffer` get the flag injected | `mesh`, `ops`, `node` |
