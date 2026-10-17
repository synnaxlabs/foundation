- **SDK DATA PATH (adopts D12 for Python)** The Python SDK wraps the Rust SDK `sdk`
  with PyO3, so the wire, the TLS pin, the hello, and the frame decode have one
  implementation. `sdk` takes its clock, entropy, network, and the shard of its client
  from `os`, so the wheels run where `os` runs: Linux, macOS, and Windows ("not cross
  compilation but full 3 platform support", the person, relayed by `laptop.monitor`,
  2026-10-16:
  https://github.com/synnaxlabs/foundation/pull/2933#issuecomment-6226448657).
  The Windows wheels of the gate (#2923) wait on the clock (#682) and the network
  (#2955) of `os` on Windows. The term that `sdk` does not depend on `os` was a proposal
  of `laptop.monitor`, not the person's words, and is withdrawn (the same link). The
  person decided on 2026-10-15 at 17:40 UTC ("Ok, let's
  do A", relayed by `laptop.monitor`, quoted at
  https://github.com/synnaxlabs/foundation/issues/2177#issuecomment-6198508264), with
  these reservations: "my main reservations for A is the end user DX and the build
  process for different OSes and makking it hard for our users to use our packages.
  But B means we need to constantly sync protocols and we might hit a performacne
  cost". Terms: the gate PR of C6 (PR 1 of its list, before any other SDK code) builds
  the wheels for Linux, macOS, and Windows on x86_64 and arm64, and installs each on a
  clean machine and connects; a failure or pain goes to the person, who can switch to B
  (the protocol again in pure Python) before other SDK code exists. `pyo3`, `maturin`,
  and `numpy` (the Rust crate) are approved. Supersedes C7 + D12 REJECTED
  (`docs/decisions/operations/c7-d12-rejected.md`) for the data path of the Python SDK.
  The base layer generated from the operation table and the native layer (C7) stay.
