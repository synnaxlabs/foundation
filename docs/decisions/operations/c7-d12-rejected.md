- **C7 + D12 rejected** Each SDK has a generated base layer and a hand-written native
  layer. Each SDK's data path is hand-written as the fastest idiomatic code for its
  language. Guard: the Rust codec is the reference; golden vectors from it run in every
  SDK's CI; differential fuzzing runs both ways; the drift agent watches it.
