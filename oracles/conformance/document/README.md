# Document conformance

Pinned bytes of the canonical Document encoding (`document::encoding`). Each vector
was written by hand from the format in `crates/document/src/encoding.rs`, not
captured from the encoder. `spec` hashes these bytes, so a vector never changes; a
new format takes a new version byte and new vectors.

`crates/document` runs them as its `conformance` test. The `[[test]]` entry in
`crates/document/Cargo.toml` is part of this oracle: to remove it is to weaken the
oracle.

```sh
cargo test -p document --test conformance
```
