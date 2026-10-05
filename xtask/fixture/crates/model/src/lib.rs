//! A crate with unsafe code, and a loom test whose cfg spans lines.

#![deny(unsafe_code)]

/// Returns the byte.
#[expect(unsafe_code, reason = "fixture")]
pub fn first(bytes: &[u8; 1]) -> u8 {
    // SAFETY: `bytes` holds one byte.
    unsafe { *bytes.as_ptr() }
}

#[cfg(all(
    test,
    loom
))]
mod tests {
    #[test]
    fn fails_under_loom() {
        panic!("expected: the xtask tests check that this loom test fails");
    }
}
