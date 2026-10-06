//! Random bytes from the OS.

/// Fills from the random source of the OS.
pub(crate) struct Driver;

impl env::entropy::Driver for Driver {
    fn fill(&self, bytes: &mut [u8]) {
        if let Err(e) = getrandom::fill(bytes) {
            panic!("the OS gives no random bytes: {e}");
        }
    }
}
