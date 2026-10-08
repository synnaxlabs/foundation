- **CPU BASELINE (2026-10-05)** Builds assume x86-64-v2 on x86-64 (every CPU since
  2009) and the CRC instruction on aarch64 Linux (Raspberry Pi 3 and later, Graviton;
  Apple chips have it already). `.cargo/config.toml` sets both, so tests, benchmarks,
  and releases build the same code. A binary fails on an older CPU. Without the flags,
  the `crc32c` kernels ran 2x slower (#140). The person decided on 2026-10-05 ("Raise
  the minimum").
