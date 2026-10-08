- **POOL COPY (#1599)** `Pool::copy(&self, bytes: &[u8]) -> Result<Block, Error>` gives
  a frozen block that holds a copy of `bytes`, with the errors of `alloc` for
  `bytes.len()`. Callers repeated `alloc`, `copy_from_slice`, and `freeze`. Lost:
  `alloc` with a closure that writes in place, because each caller already holds its
  bytes as a slice. Decided by `laptop.architect` (2026-10-07T20:43:02Z):
  https://github.com/synnaxlabs/foundation/issues/1599#issuecomment-6046480683
