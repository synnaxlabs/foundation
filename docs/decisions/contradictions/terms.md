# Terms used two ways

**X47. "Lease".** It means a node lease (failover), a control lease (writer setting),
and r12's `endpoint::Lease<T>`. Resolution: in prose, always "node lease" or "control
lease". Rename `endpoint::Lease<T>` to `endpoint::Handle<T>`. (Names delegation.)

**X48. "Slot".** It means `channel::Slot`, the latest-value slot (r8, B4), a kind
table slot (C5 SHAPE), and a memtable slot (r2). Resolution: "slot" means only
`channel::Slot`. Use "latest mailbox", "table entry", and "memtable entry". (Names
delegation.)

**X49. "Kind".** It means the channel enum (`Kind::Index`, `Kind::Data`), connector
kinds (`kind = "opcua"`), and policy kinds. Resolution: user-facing "kind" means a
connector kind only. Prose says "index channel" and "data channel", never "channel
kind". The internal `spec::channel::Kind` stays namespaced. (Names delegation.)

**X50. "Block".** It means a Document block (HCL) and a pool buffer (`block::Block`).
Resolution: keep both, namespaced (`document::Block`, `block::Block`). Prose says
"config block" and "pool block". (Names delegation; flagged in the log, BQ21.)

**X51. "Integration", "sink", "durable reader", "mesh file".** These retired terms still
appear in A11, A16, A19, B1, S10, K4, C8, and the reports. Resolution: read
"integration" and "sink" as "out connector", "durable reader" as "named reader with a
hold", and "mesh file" as "definition files".

**X52. Decision IDs.** r9 numbers its decisions D1 to D14, which collide with the log's
D1 to D7; r10 uses D1 to D8; r12 and r13 number theirs too. Resolution: cite them as
R9-Dn, R10-Dn, R12-n, and R13-n (as this record does).

**X53. D6 path lock vs C9c.** D6 says agents "cannot edit" contract and oracle paths.
T2 calls that too strict, and C9c enforces oracles by visibility only. Resolution: C9c.
People still own contracts and oracles; agents may edit them, and every weakening gets
an adversarial reviewer and a person's merge.

**X54. "Clock".** It means `env::clock::Clock`, the monotonic clock of one node, and
the `clock` crate, which serves mesh time. Resolution: in prose, "monotonic clock" for
the `env` seam and "mesh clock" for what the `clock` crate serves.

Count: 54 items (X1 to X54).
