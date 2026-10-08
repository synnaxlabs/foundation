- **C9d** Releases follow RFC 0058: dispatch from main or `release/X.Y`; tag `vX.Y.Z` in
  this repository; `-rc.N` never counts as shipped. One binary per target (Linux x86-64
  and ARM, macOS, Windows), each tested on real hardware. `foundation upgrade <version>`
  is an ordinary operation, rolling one node at a time; nodes fetch the signed binary by
  hash from a nearby peer. Each wire and disk format has one integer version; a node
  reads its own and the previous one; new formats turn on only after every node runs the
  release. Compatibility is owed only to stable releases. Until the first stable
  release, each wire and disk format stays at version 1, and a breaking change does not
  add a version. The person decided on 2026-10-05 ("keep version 1. we should only make
  breaking changes until we release v1"), #374.
