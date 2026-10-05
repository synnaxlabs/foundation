//! Waits between attempts that fail.

use env::clock::Clock;
use env::rng::Rng;
use types::time::Span;

use crate::cancel;

/// Waits between attempts with capped exponential backoff and full jitter. The nth
/// wait since the last reset is a random span in `0..=min(cap, first * 2^n)`, so many
/// connectors that fail at once do not retry at once.
///
/// ```
/// use connector::retry::{Backoff, Config};
/// use types::time::Span;
///
/// use connector::cancel::Token;
/// use env::{clock::Clock, rng::Rng};
///
/// async fn serve(clock: &Clock, rng: Rng, cancel: &Token) {
///     let config = Config { first: Span::SECOND, cap: Span::MINUTE };
///     let mut backoff = Backoff::new(clock, rng, config);
///     loop {
///         if session().is_ok() {
///             backoff.reset();
///         }
///         if !backoff.wait(cancel).await {
///             return;
///         }
///     }
/// }
/// # fn session() -> Result<(), ()> { Ok(()) }
/// ```
#[derive(Debug)]
pub struct Backoff {
    clock: Clock,
    rng: Rng,
    /// At least zero.
    cap: Span,
    /// In `1 ns..=cap`, or zero when `cap` is zero.
    first: Span,
    /// The longest span the next wait can take. In `first..=cap`.
    ceiling: Span,
}

/// The spans of a [`Backoff`]. A `first` below 1 ns counts as 1 ns, so the waits
/// always grow to `cap`. A `cap` below zero counts as zero: no wait at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// The longest first wait.
    pub first: Span,
    /// The longest wait.
    pub cap: Span,
}

impl Backoff {
    /// Makes a backoff whose next wait is its first.
    #[must_use]
    pub fn new(clock: &Clock, rng: Rng, config: Config) -> Self {
        let cap = config.cap.max(Span::ZERO);
        let first = config.first.max(Span::from_nanos(1)).min(cap);
        Self {
            clock: clock.clone(),
            rng,
            cap,
            first,
            ceiling: first,
        }
    }

    /// Waits for the next span. Returns `true` when the wait ends, or `false` at once
    /// when `cancel` is cancelled.
    ///
    /// # Panics
    ///
    /// On a thread that `env` did not start.
    pub async fn wait(&mut self, cancel: &cancel::Token) -> bool {
        let ceiling = u64::try_from(self.ceiling.nanos()).expect("at least zero");
        let span =
            i64::try_from(self.rng.below(ceiling + 1)).expect("at most i64::MAX");
        self.ceiling =
            Span::from_nanos(self.ceiling.nanos().saturating_mul(2)).min(self.cap);
        cancel
            .race(self.clock.sleep(Span::from_nanos(span)))
            .await
            .is_some()
    }

    /// Makes the next wait the first again. Call it after an attempt that worked.
    pub fn reset(&mut self) {
        self.ceiling = self.first;
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::cancel::Token;
    use crate::common::run;
    use env::rng::Rng;

    fn ms(n: i64) -> Span {
        Span::from_nanos(n * 1_000_000)
    }

    /// Waits `n` times, resetting before each wait in `resets`, and returns the span
    /// of each wait.
    fn waits(seed: u64, config: Config, n: usize, resets: Vec<usize>) -> Vec<Span> {
        run(move |clock, _, _| async move {
            let token = Token::new();
            let mut backoff = Backoff::new(&clock, Rng::from_seed(seed), config);
            let mut out = Vec::new();
            for i in 0..n {
                if resets.contains(&i) {
                    backoff.reset();
                }
                let start = clock.now();
                assert!(backoff.wait(&token).await, "a live token");
                out.push(clock.now() - start);
            }
            out
        })
    }

    #[test]
    fn grows_to_the_cap_and_spreads_below_it() {
        let config = Config {
            first: ms(1),
            cap: Span::SECOND,
        };
        let spans = waits(7, config, 400, Vec::new());
        // From the 11th wait on, the ceiling is the cap: 1 ms * 2^10 > 1 s.
        let late = spans.get(10..).expect("400 waits");
        let mean = late.iter().map(|s| s.nanos()).sum::<i64>() / 390;
        assert!(
            (ms(400).nanos()..=ms(600).nanos()).contains(&mean),
            "uniform up to the cap: mean {mean} ns"
        );
        assert!(late.iter().any(|s| *s > ms(900)), "reaches near the cap");
    }

    #[test]
    fn starts_again_from_the_first_after_a_reset() {
        let config = Config {
            first: ms(1),
            cap: Span::HOUR,
        };
        let spans = waits(5, config, 64, (32..64).collect());
        assert!(
            spans
                .get(..32)
                .expect("64 waits")
                .iter()
                .any(|s| *s > Span::SECOND)
        );
        assert!(
            spans
                .get(32..)
                .expect("64 waits")
                .iter()
                .all(|s| *s <= ms(1))
        );
    }

    #[test]
    fn returns_false_at_once_when_cancelled_during_a_wait() {
        let (waited, elapsed) = run(|clock, tasks, _| async move {
            let token = Token::new();
            let canceller = token.clone();
            let sleeper = clock.clone();
            tasks.spawn(async move {
                sleeper.sleep(ms(50)).await;
                canceller.cancel();
            });
            let config = Config {
                first: Span::HOUR,
                cap: Span::HOUR,
            };
            let mut backoff = Backoff::new(&clock, Rng::from_seed(1), config);
            let start = clock.now();
            let waited = backoff.wait(&token).await;
            (waited, clock.now() - start)
        });
        assert!(!waited, "cancelled");
        assert_eq!(elapsed, ms(50), "at the cancel");
    }

    #[test]
    fn returns_false_without_a_wait_when_already_cancelled() {
        let (waited, elapsed) = run(|clock, _, _| async move {
            let token = Token::new();
            token.cancel();
            let config = Config {
                first: Span::HOUR,
                cap: Span::HOUR,
            };
            let mut backoff = Backoff::new(&clock, Rng::from_seed(1), config);
            let start = clock.now();
            (backoff.wait(&token).await, clock.now() - start)
        });
        assert!(!waited, "cancelled");
        assert_eq!(elapsed, Span::ZERO);
    }

    #[test]
    fn grows_again_after_a_reset() {
        let config = Config {
            first: ms(1),
            cap: Span::HOUR,
        };
        let spans = waits(5, config, 62, vec![32]);
        let late = spans.get(32..).expect("62 waits");
        assert!(late.iter().any(|s| *s > Span::SECOND));
    }

    #[test]
    fn can_wait_the_whole_ceiling() {
        let config = Config {
            first: Span::from_nanos(1),
            cap: Span::from_nanos(1),
        };
        let spans = waits(9, config, 64, Vec::new());
        assert!(spans.contains(&Span::from_nanos(1)));
    }

    #[test]
    fn grows_from_one_nanosecond_when_the_first_is_zero() {
        let config = Config {
            first: Span::ZERO,
            cap: Span::MINUTE,
        };
        let spans = waits(4, config, 64, Vec::new());
        assert!(spans.iter().any(|s| *s > Span::SECOND));
    }

    #[test]
    fn never_waits_when_the_cap_is_below_zero() {
        let config = Config {
            first: ms(-5),
            cap: ms(-1),
        };
        assert_eq!(waits(3, config, 4, Vec::new()), [Span::ZERO; 4]);
    }

    proptest! {
        #[test]
        fn keeps_every_wait_under_its_ceiling(
            seed: u64,
            first in 0..1_000_000_000_i64,
            cap in 0..60_000_000_000_i64,
            resets in prop::collection::vec(0..80_usize, 0..4),
        ) {
            let config = Config {
                first: Span::from_nanos(first),
                cap: Span::from_nanos(cap),
            };
            let spans = waits(seed, config, 80, resets.clone());
            let mut ceiling = first.max(1).min(cap);
            for (i, span) in spans.into_iter().enumerate() {
                if resets.contains(&i) {
                    ceiling = first.max(1).min(cap);
                }
                prop_assert!(span >= Span::ZERO, "wait {i} is not negative");
                prop_assert!(span.nanos() <= ceiling, "wait {i}: {span:?} > {ceiling}");
                ceiling = ceiling.saturating_mul(2).min(cap);
            }
        }
    }
}
