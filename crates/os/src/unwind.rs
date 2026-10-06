//! Catches panics, and the panics in the drops of their payloads.

use std::any::Any;
use std::mem;
use std::panic::{self, AssertUnwindSafe};

/// The most payloads that [`discard`] drops in one chain.
const CHAIN: usize = 16;

/// Calls `f` and gives its value, or `None` when it panics. The payload of the panic
/// drops as [`discard`] drops it. `f` need not be unwind safe: the caller may only
/// drop what a panic of `f` left half done.
pub(crate) fn catch<T>(f: impl FnOnce() -> T) -> Option<T> {
    panic::catch_unwind(AssertUnwindSafe(f))
        .map_err(discard)
        .ok()
}

/// Drops the payload of a panic. A panic in its drop is caught, and the payload of
/// that panic drops the same way, up to [`CHAIN`] payloads. It forgets a payload past
/// them, so that a drop that always panics cannot hang the thread.
fn discard(payload: Box<dyn Any + Send>) {
    let mut next = Some(payload);
    for _ in 0..CHAIN {
        let Some(payload) = next else {
            return;
        };
        next = panic::catch_unwind(AssertUnwindSafe(|| drop(payload))).err();
    }
    #[expect(clippy::mem_forget, reason = "its drop may panic again")]
    mem::forget(next);
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    /// A panic payload whose drop panics with a `Relay` of one less `left`, until
    /// `left` is 0. Each drop adds 1 to `drops`.
    pub(crate) struct Relay {
        pub(crate) left: usize,
        pub(crate) drops: Arc<AtomicUsize>,
    }

    impl Drop for Relay {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            if self.left > 0 {
                let drops = Arc::clone(&self.drops);
                panic::panic_any(Self {
                    left: self.left - 1,
                    drops,
                });
            }
        }
    }

    #[test]
    fn catch_gives_the_value_of_a_call_that_returns() {
        assert_eq!(catch(|| 7), Some(7));
    }

    #[test]
    fn catch_drops_each_payload_of_a_panic_whose_payloads_panic_in_their_drops() {
        let drops = Arc::new(AtomicUsize::new(0));
        let relay = Relay {
            left: 3,
            drops: Arc::clone(&drops),
        };
        assert_eq!(catch(|| panic::panic_any(relay)), None::<()>);
        assert_eq!(drops.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn catch_forgets_a_payload_past_the_chain() {
        let drops = Arc::new(AtomicUsize::new(0));
        let relay = Relay {
            left: CHAIN,
            drops: Arc::clone(&drops),
        };
        assert_eq!(catch(|| panic::panic_any(relay)), None::<()>);
        assert_eq!(drops.load(Ordering::SeqCst), CHAIN);
    }
}
