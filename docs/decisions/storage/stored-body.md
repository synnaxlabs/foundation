- **STORED BODY (#191)** The bytes of a data entry (S4) are `[count: u32]`, then
  `[channel: u128][kind: u8][element: u8][n: u32][end: u32]` for each present series
  of the index frame in entry order, then the frame's encoded series bytes,
  little-endian. `end` is as in FRAME LAYOUT. Kinds: scalar 0, array 1, list 2, string
  3, bytes 4, matrix 5. `n` is the array length, the list maximum, or `rows | columns
  << 16` of a matrix, else 0. `element` is the scalar (bool 0, i8 1, i16 2, i32 3, i64
  4, u8 5, u16 6, u32 7, u64 8, f32 9, f64 10, stamp 11, span 12, uuid 13), else 0. The
  type is the writer's type, so a reader decodes with it after an `apply` changes the
  channel's type (A15). The header is one pool block and the series bytes are a view of
  the frame's block, so a write copies no series byte. A slot or key set number is never
  stored. The layout is part of the disk format version (C9d), as in FRAME LAYOUT. Copy
  mode checks each stored body once where remote records enter (X43), and the read after
  it panics on a bad body. Decided by the `write-path` builder; approved by the
  coordinator (#191).
  Amended: kind 5, with both `u16` sides in `n`, so the descriptor stays 26 bytes and
  a body with no matrix is as before. Decided by `laptop.architect`
  (2026-10-07T17:30:55Z):
  https://github.com/synnaxlabs/foundation/issues/1341#issuecomment-6043244011.
  Supersedes the `columns` table of
  https://github.com/synnaxlabs/foundation/issues/1341#issuecomment-6042293625.
