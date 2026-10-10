- **NODE FUZZ (#1994, 2026-10-09)** The fuzz target `node_identity` checks the decode
  of `node.key`, which T1 asks for, since the file is outside input. It calls
  `node::fuzz::identity`, a function in a `#[doc(hidden)]` public module behind the
  feature `sim`, as `bench` is, and `fuzz/Cargo.toml` turns `sim` on. The target takes
  100 bytes as they are, or 96 bytes with their CRC32C appended, since a random input
  almost never has the right CRC32C. The oracle is `identity::check`, which the
  property tests share: `decode` gives an identity exactly for bytes with the tag and
  the CRC32C, and that identity encodes to the same bytes. `check` tests the first
  part as the bytes that `encode` writes from the fields of the input. The inputs of
  the form `foundation/key/1` stay, and reach only the length check since the form
  `foundation/key/2` (NODE PORT, #1744). Lost: property
  tests only, with a sentence in `docs/security.md` that the decode needs no target.
  It breaks T1. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/1994#issuecomment-6071757310,
  2026-10-09 00:28 UTC); the surface, in
  https://github.com/synnaxlabs/foundation/pull/2046#issuecomment-6071891219
  (2026-10-09 00:41 UTC); `check` as a property of `encode`, by `laptop.director`
  (https://github.com/synnaxlabs/foundation/pull/2046#issuecomment-6072502872,
  2026-10-09 01:39 UTC).
