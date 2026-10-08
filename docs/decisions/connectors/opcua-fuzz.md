- **OPCUA FUZZ (#1885)** `fuzz/` holds no `unsafe`. The `connector_opcua_decode`
  target only calls `connector_opcua::fuzz::decode`, a function in a
  `#[doc(hidden)]` public module. It uses the private `ffi` bindings of the crate,
  with one unsafe operation and a `// SAFETY:` comment in each block. A unit test in
  the crate checks the names of entries 0, 14, and 387 of `UA_TYPES`, and another runs
  `decode` on each file in `oracles/fuzz/connector_opcua_decode/`. Until #435 fixes the
  checks of a `Variant` of `ExtensionObject` values, `decode` pads with zeros and
  decodes again; a unit test fails when the fix lands. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/1885#issuecomment-6068183610,
  2026-10-08 20:12 UTC). It pads only an encoding that does not decode, so each other
  encoding must decode alone (round 1 of #1997,
  https://github.com/synnaxlabs/foundation/pull/1997#issuecomment-6069158609). The
  module is behind the feature `sim`, as `bench` is, and `fuzz/Cargo.toml` turns `sim`
  on. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/1885#issuecomment-6068286559,
  2026-10-08 20:18 UTC). Two bytes, a little-endian `u16`, pick the type, so each of
  the 388 types is reachable. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/1885#issuecomment-6068291471,
  2026-10-08 20:18 UTC).
