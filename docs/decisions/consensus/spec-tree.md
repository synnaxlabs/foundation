- **SPEC TREE (#6)** `spec::tree` is the prolly tree of one region. A key is a full
  name in byte order, so the descendants of one name are one range. A value is opaque
  bytes. A chunk is a level byte, then entries: a leaf entry is a key and a value, and
  an entry above is the last key of a child and its BLAKE3 hash. A chunk ends after an
  entry when a draw from the BLAKE3 hash of the level and the key is below
  `(end^4 - start^4) / 4096^4`, where `start` and `end` are the entry's byte offsets
  in the chunk (Weibull hazard, shape 4), or when the chunk reaches 16 KiB. A chunk
  above the leaves holds at least two entries, unless it is the last of its level, so
  a key of any size fits. The rule uses integers only. A chunk with one child is
  never a root, so the tree is a function of its entries. The empty tree has the root
  `tree::empty()` and no stored chunk. Chunks come from peers, so a reader checks each
  chunk that it reads: keys in order, each length in its shortest form, and a child at
  the level below with the last key that its parent gives and a first key above each key
  that comes before it in the chunks above (#684). A chunk that fails gives
  `Error::Corrupt(hash)`. A reader does not check the boundaries, and only `diff` checks
  that a leaf key is a name, so "a function of its entries" holds for trees that `apply`
  made. The tree does no I/O: the caller fills a `tree::Chunks`, and `get`, `apply`, and
  `diff` return `Error::Missing(hash)` for a chunk that is not there, so the caller
  fetches it and runs the operation again. Each run names one chunk, because a change
  record lists the chunks that it made and a caller fetches those first. `apply` takes a
  batch of `tree::Change` values, adds the new chunks to the `Chunks`, and returns the
  new root and their hashes. `diff` returns each changed entry with its old and new
  value, and the chunks that only the new tree has. A read of all entries below one name
  is a later function of `spec::tree`; it replaces the `spec::Tree::region` of X12. A
  chunk has no maximum size: one value is in one chunk, and the limit on a value belongs
  to the code that encodes definitions. A chunk's address is a `types::digest::Digest`,
  the same type that `wire` and `blob` carry. To change the chunk format or the boundary
  rule changes every root digest.
