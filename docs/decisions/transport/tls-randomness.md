- **TLS RANDOMNESS (2026-10-04)** All randomness inside TLS (key shares, client
  random, nonces) comes from aws-lc, not from `env`. rustls holds its random source
  as a `&'static` value, and aws-lc makes X25519 key shares with its own randomness,
  so neither can be injected without a leak per `Transport`. It changes bytes, never
  sizes or timing. Simulated runs still replay because nothing branches on those
  bytes; replay traces leave out ciphertext and handshake randoms, and a `sim` test
  runs one value twice and compares the traces. The person accepted it ("Accept TLS
  RANDOMNESS"), from `network`'s proposal on #54.
  #2934 extends it to the OPC UA secure channel of `connector-opcua`: the OpenSSL
  plugins of open62541 draw nonces, keys, padding, salts, and the blinding of RSA from
  aws-lc (`RAND_bytes`), not from `env`, with the same bounds: bytes only, and nothing
  branches on them. Simulated runs still replay: replay traces leave out the bytes of
  a secured channel, and a `sim` test of #2934 PR 1b reads one value twice in
  `SignAndEncrypt` and compares the traces. `AWS_LC_KEYS` of `cargo xtask open62541`
  keys each call of aws-lc that draws by its file and function, and `AWS_LC`, the
  functions that any file may call, holds none that draws. OK from `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6230838999,
  2026-10-17 06:46 UTC). The person, 2026-10-17 at about 16:08 UTC, through
  `laptop.monitor`: "Yes", to this extension with the draws above and to the exception
  of architecture rule 2 of `CLAUDE.md` for the OPC UA secure channel
  (https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6236293785).
