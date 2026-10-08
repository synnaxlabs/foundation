- **SECTOR (2026-10-05)** `env::files::SECTOR` (512) is the length of the sector that
  a crash keeps or loses whole in a write that is not yet durable. It is a constant,
  so that a store format asserts against it when it compiles. A length read from the
  device at run time lost (#569).
