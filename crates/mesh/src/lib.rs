//! Agrees per region, through `raft`, on spec pointers, delegations, and runtime state
//! (membership, node leases, homes, seq blocks, index history, secret ciphertexts,
//! tickets, versions, rollout lock, format flag); serves snapshots, watches, effective
//! settings, and the changes channels.
