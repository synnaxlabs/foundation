- **SDK PYTHON** The Python SDK is the crate `sdk-python`, a PyO3 module
  `foundation._native` of the wheel `synnax-foundation` that wraps `sdk::Client`
  (`laptop.architect`, 2026-10-17,
  https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6238783320;
  `laptop.architect-2`, 2026-10-17T19:07:21Z,
  https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6239220977).
  `connect(address, *, node, public_key, subject, private_key)` takes the node key as a
  `uuid.UUID`, and a value of another type raises `TypeError`. `sdk::Error::Thread`
  raises `RuntimeError`, `Bind` raises `OSError`, `Session` raises `ConnectionError`,
  and `Panicked` raises `PanicException` (`laptop.architect-2`, 2026-10-17T18:44:19Z,
  https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6238826510).
  `Client.__exit__` lets the error of the block win: when the block raised, an error of
  the close is a note on it, else the error of the close raises (`laptop.monitor`, for
  the person; `laptop.architect-2`, 2026-10-17T18:46:12Z,
  https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6238857409). The
  feature `sim` adds test-only functions, so a test asserts that a panic on the caller's
  thread gives `PanicException`, not an abort (`laptop.architect-2`,
  2026-10-17T18:39:00Z,
  https://github.com/synnaxlabs/foundation/issues/2923#issuecomment-6238735903). One
  `abi3` wheel for each platform takes Python 3.11 and later, and does not install on
  free-threaded Python (`laptop.monitor`, key choice of kind (a), accepted
  2026-10-17).
