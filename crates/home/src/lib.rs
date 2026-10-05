//! Runs the per-index write path (time checks, seq, fence, control, storage, fan-out),
//! crash-recovery and copy-mode opens, and companion writes.

#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the frame path is the first user")
)]
mod index;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the frame path is the first user")
)]
mod order;
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the frame path is the first user")
)]
mod stored;
