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
/// async fn connect(clock: &Clock, rng: Rng, cancel: &Token) {
///     let config = Config { first: Span::SECOND, cap: Span::MINUTE };
///     let mut backoff = Backoff::new(clock, rng, config);
///     while try_once().is_err() {
///         if !backoff.wait(cancel).await {
///             return;
///         }
///     }
///     backoff.reset();
/// }
/// # fn try_once() -> Result<(), ()> { Ok(()) }
/// ```
#[derive(Debug)]
pub struct Backoff {
    clock: Clock,
    rng: Rng,
    config: Config,
    /// The longest span the next wait can take.
    ceiling: Span,
}

/// The spans of a [`Backoff`]. A span below zero counts as zero.
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
        Self {
            clock: clock.clone(),
            rng,
            config,
            ceiling: first(config),
        }
    }

    /// Waits for the next span. Returns `true` when the wait ends, or `false` at once
    /// when `cancel` is cancelled.
    ///
    /// # Panics
    ///
    /// On a thread that `env` did not start.
    pub async fn wait(&mut self, cancel: &cancel::Token) -> bool {
        let nanos = self.ceiling.nanos().unsigned_abs();
        let span = self.rng.below(nanos + 1);
        let span = Span::from_nanos(i64::try_from(span).expect("at most the ceiling"));
        let cap = Span::from_nanos(self.config.cap.nanos().max(0));
        self.ceiling =
            Span::from_nanos(self.ceiling.nanos().saturating_mul(2)).min(cap);
        cancel.race(self.clock.sleep(span)).await.is_some()
    }

    /// Makes the next wait the first again. Call it after an attempt that worked.
    pub fn reset(&mut self) {
        self.ceiling = first(self.config);
    }
}

fn first(config: Config) -> Span {
    Span::from_nanos(config.first.nanos().min(config.cap.nanos()).max(0))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use env::tasks::Tasks;
    use proptest::prelude::*;

    use super::*;
    use crate::cancel::Token;
    use env::rng::Rng;

    /// Runs `main` on a shard of one simulated node and returns its output.
    fn run<T, F>(main: impl FnOnce(Clock, Tasks) -> F + Send + 'static) -> T
    where
        T: Send + 'static,
        F: Future<Output = T> + 'static,
    {
        let mut sim = sim::Sim::new(sim::Config::default());
        let node = sim.node(sim::node::Config::default());
        let clock = node.clock();
        let out = Arc::new(Mutex::new(None));
        let slot = Arc::clone(&out);
        let config = env::shards::Config {
            name: "shard-0".into(),
            core: Some(0),
        };
        let handle = node
            .shards()
            .start(config, move |tasks| async move {
                let value = main(clock, tasks).await;
                *slot.lock().expect("no panic under the lock") = Some(value);
            })
            .expect("the shard starts");
        sim.run().expect("the run ends");
        handle.join().expect("the shard ends");
        let value = out.lock().expect("no panic under the lock").take();
        value.expect("main returned")
    }

    fn ms(n: i64) -> Span {
        Span::from_nanos(n * 1_000_000)
    }

    /// Waits `n` times, resetting before each wait in `resets`, and returns the span
    /// of each wait.
    fn waits(seed: u64, config: Config, n: usize, resets: Vec<usize>) -> Vec<Span> {
        run(move |clock, _| async move {
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
        let (waited, elapsed) = run(|clock, tasks| async move {
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
        let (waited, elapsed) = run(|clock, _| async move {
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
    fn counts_a_span_below_zero_as_zero() {
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
            let mut ceiling = first.min(cap);
            for (i, span) in spans.into_iter().enumerate() {
                if resets.contains(&i) {
                    ceiling = first.min(cap);
                }
                prop_assert!(span >= Span::ZERO, "wait {i} is not negative");
                prop_assert!(span.nanos() <= ceiling, "wait {i}: {span:?} > {ceiling}");
                ceiling = ceiling.saturating_mul(2).min(cap);
            }
        }
    }
}
