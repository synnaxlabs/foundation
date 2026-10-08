- **MESH LOG (#471)** `mesh` keeps the `raft` hard state and log of a region in the
  files `log-0`, `log-1`, and so on of one directory. A log also holds the file `lock`
  of that directory open for writing, from its open until it drops. A file call of a
  dropped write can end after the drop, and the lock does not cover it (#1375). The file
  has no bytes, and one with bytes fails the open (`Files(Length)`). So a second open of
  the directory gives `Error::Log` with `Busy` on the lock, at each time, whatever log
  file the first log holds (#1360, decided by `laptop.architect`, 2026-10-07T12:07:03Z:
  https://github.com/synnaxlabs/foundation/issues/1360#issuecomment-6037555764). Any
  other file there is `Error::Stray`. Supersedes
  https://github.com/synnaxlabs/foundation/pull/549 in its clause that each file but
  `log-<n>` is `Error::Stray`. One write of `raft` is one record: a header, then the
  body. The header is an 8-byte check of the rest of the header (the first bytes of
  `types::digest::Digest::of`), the format version (1, C9d), the record's number, the
  body length, and an 8-byte check of the body. The body holds the hard state, when it
  changed, and the entries, so one sync makes both durable; two slots for the hard state
  lost, because they need a second sync and a second torn-write rule. The hard state is
  the term, then the vote, the leader, and the proof, each behind a presence byte; the
  proof is a grant byte, the candidate, a count of voters, then each voter's key (16
  bytes) and signature (64 bytes) in rising key order (#750). A `raft` message on the
  wire carries its proof in the same form, after the term and before the body, then
  its chain: an 8-byte count of links, always present, then each link's position (8
  bytes of term, 8 of index) and its change in the entry form below, from the
  incoming keys to the signature. A link of a joint entry with 3 incoming, 3
  outgoing, and 3 signed votes is 457 bytes, and a chain of 2 such links is 922
  bytes with its count (architect, #881,
  https://github.com/synnaxlabs/foundation/issues/881#issuecomment-6030969579,
  2026-10-07T04:31:40Z). A
  granted `PreVoteReply` or `VoteReply` is the byte 1, then the signature; a refusal
  is the byte 0 alone. An entry is its term and index (8 bytes each), then a data
  byte: empty (0) alone; bytes (1), an 8-byte length, and the bytes; voters (2), the
  incoming keys, the outgoing keys (each an 8-byte count, then the keys in rising
  order), the votes in the proof form, and the leader's signature (64 bytes). No
  form holds a grant or a change with no signature: encode panics on one, because
  the caller signs before each write and send. `mesh::claim` signs each claim with
  the node's Ed25519 key. A grant signs `foundation/grant/1`, the voter (16 bytes,
  little endian), the grant byte (pre-vote 0, vote 1), the term (8 bytes, little
  endian), and the candidate (16 bytes, little endian). A change signs
  `foundation/voters/1`, the leader (16 bytes), the term and the index (8 bytes
  each), and the incoming and the outgoing keys, each with its count as in the
  entry, not the 4-byte count of the plan: one form for both (architect, #881,
  https://github.com/synnaxlabs/foundation/pull/1187#issuecomment-6032591908).
  The signer in the bytes keeps two members that share a key from sharing a
  signature. Grants name no region; a second region adds the region key under
  `foundation/grant/2`. The driver (#471) checks each
  claim of a message before each `step` against the key of its signer: the key of a
  member in the applied state, else the key that each join of that node in the log as
  `raft` holds it and not applied names, when all of them name one key (decided by
  `laptop.director`, 2026-10-07T12:48:00Z and 2026-10-07T13:10:50Z:
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038235423 and
  https://github.com/synnaxlabs/foundation/issues/1382#issuecomment-6038649429). An
  entry that replaces a join removes its key, at the step that replaces it. Two joins
  that name two keys: MESH DRIVER states the rule. The log
  is the one that `raft` reads its configuration from, so each `step` and each proposal
  syncs the keys from `Raft::unstable` before the write (decided by `laptop.architect`,
  2026-10-07T13:31:26Z:
  https://github.com/synnaxlabs/foundation/pull/1400#issuecomment-6039027801). The same
  key serves the check of the sender of a message (`Error::Spoofed`) and of the peer of
  a forwarded proposal (`Error::PeerNotVoter`) (decided by `laptop.architect`,
  2026-10-07T13:23:59Z:
  https://github.com/synnaxlabs/foundation/pull/1400#issuecomment-6038887227). The
  format version stays 1: no log has shipped. A later record replaces the
  entries from its first index. A file is 1 MiB, or the length of the record that the
  log made it for when that is more. A record that does not fit starts the next file.
  In a file with no record, it makes that file again, larger, so each file but the last
  holds a record. A file with no bytes, which a crash in a create can leave (ENV SEAMS,
  #1264), is a file with no record. After a stopped write that made a file, the next
  record starts that file. Decided by `laptop.architect` (2026-10-07T10:38:51Z, the
  last sentence at 2026-10-07T11:05:15Z, and the sentence on a file with no bytes at
  2026-10-07T18:39:26Z):
  https://github.com/synnaxlabs/foundation/pull/1284#issuecomment-6036182314,
  https://github.com/synnaxlabs/foundation/pull/1284#issuecomment-6036600297, and
  https://github.com/synnaxlabs/foundation/issues/1264#issuecomment-6044422561. A write
  puts its record in blocks, one block of the pool at a time and of 64 KiB at most, from
  the end of the record to its start, and then syncs one time. Each block but the one at
  the end of the record ends at a multiple of the block size in the file, so no two
  blocks share a sector. The block with the header is the last that it writes, so a
  write that the pool stops (`Error::Pool`) leaves no header: the log holds what it
  held, and the bytes of the stopped write stay after its end. Decided by
  `laptop.architect` (2026-10-07T09:12:02Z):
  https://github.com/synnaxlabs/foundation/pull/1284#issuecomment-6034784720. So a pool
  that opens holds each write. A write that a file call fails, or that its caller drops,
  poisons the log (`Error::Poisoned`), and a write that the pool stops does not. The
  next write puts zeros over the bytes of the stopped write and syncs, and only then
  writes its record: with one sync, a power cut can keep the record and not the zeros
  (SIM CRASH), and an open then reads the old bytes as a header (PR 1 of #1091, approved
  by the architect, 2026-10-07T05:29:41Z:
  https://github.com/synnaxlabs/foundation/issues/1091#issuecomment-6031627973, and the
  text of this rule, 2026-10-07T08:42:04Z:
  https://github.com/synnaxlabs/foundation/pull/1284#issuecomment-6034282653). A header
  never crosses a `SECTOR`: a record whose header would cross one starts at the next
  sector. A power cut keeps each sector whole or not at all (SIM CRASH), so a header is
  whole or absent. At a restart, zeros where a record should start, or a good header
  with a torn body, are the end of the log. Anything else is `Error::Corrupt`, and the
  node does not start. So is a record that starts right after a torn one, or at the
  start of the next file: the error is at the torn record, whatever the version of the
  record after it. A record of another version that is the first defect in file order
  is `Error::Version` (#1784, approved by the architect, 2026-10-08T04:26:06Z:
  https://github.com/synnaxlabs/foundation/pull/1776#issuecomment-6052206016). Open
  writes again, whole, the end file that it finds: the records as it read them, then
  zeros to the end of the file. So a torn record leaves nothing that a later open reads
  as a header. Each read and each write of the open is whole sectors, so a header gets
  one write. An open with a pool whose largest block is less than one sector gives
  `Error::Pool(TooLarge)` before it reads or makes a file. Then it syncs the end file,
  the directory, and its parent, because `raft` acts on what open gives and a crash can
  leave any of them with no sync. The write is there because a read sees, from the
  cache, the writes that a failed sync of this boot lost, and a later sync does not
  write them (SIM CRASH): an open that only syncs gives records, or keeps zeros, that
  the disk does not hold (#1066; the ring has the same rule, #698). Each file before the
  end file is durable, because a failed write poisons the log, and the next open has the
  file of that write as its end file or removes it. An open of a log that has a file
  thus writes and syncs 1 MiB or more, for each region. P1 gives a Raspberry Pi 4 under
  1 s to start, and no one has measured this cost there (#1140). Lost: zeros only after
  a torn end (the first shape), which is the defect; and a read with direct I/O, which
  not each driver can give: macOS does not promise a read that skips the cache (decided
  by the architect, #1128, 2026-10-07T05:37:30Z:
  https://github.com/synnaxlabs/foundation/issues/1128#issuecomment-6031715225). One
  check over the whole record lost: a damaged length then reads as a torn end, and the
  log drops the good records after it. Zeros over the header of a durable record, which
  only a disk fault makes, read as the end, and open drops the records after it in that
  file. A search past the end for a record lost: a body can hold the bytes of a record,
  so a power cut could then stop the node. Nothing trims the log until snapshots (#253).
  `mesh` depends on `block` for the blocks of its file calls. Decided by `consensus`.
  `mesh::testing::round_trip_log_record` and `seal_log_record`, behind the `sim`
  feature, give the fuzz target `mesh_log` the decode and encode of one record; the
  seal writes the length and both checks, with the log's own check (approved by the
  architect, 2026-10-08T01:49:20Z:
  https://github.com/synnaxlabs/foundation/issues/1711#issuecomment-6050509924; the
  doc of the seal that writes the length, 2026-10-08T03:38:57Z:
  https://github.com/synnaxlabs/foundation/pull/1740#issuecomment-6051674927. Supersedes
  https://github.com/synnaxlabs/foundation/issues/1711#issuecomment-6050509924 for the
  doc of the seal).
