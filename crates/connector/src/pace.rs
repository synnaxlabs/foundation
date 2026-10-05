//! A timer that ticks at a fixed rate without drift.

use env::clock::{Clock, Sleep};
use types::time::{Monotonic, Rate, Span};

use crate::cancel;

/// Ticks at a fixed rate on a grid of deadlines that starts when the timer is made.
/// Each deadline is `start + n / rate`, computed from `n`, so the grid never drifts.
/// After a stall, the timer skips the ticks it missed and counts them.
///
/// ```
/// use connector::cancel::Token;
/// use connector::pace::Timer;
///
/// async fn sample(clock: &env::clock::Clock, cancel: &Token) {
///     let rate = types::time::Rate::new(100, 1).expect("100 Hz is a rate");
///     let mut timer = Timer::new(clock, rate);
///     while let Some(tick) = timer.tick(cancel).await {
///         let _ = tick.n;
///     }
/// }
/// ```
#[derive(Debug)]
pub struct Timer {
    clock: Clock,
    rate: Rate,
    start: Monotonic,
    /// The number of the next tick that can return.
    next: u64,
    sleep: Sleep,
}

/// One tick of a [`Timer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tick {
    /// The tick number on the grid. Tick 0 is at the start.
    pub n: u64,
    /// The ticks skipped since the last tick, because their deadlines passed.
    pub missed: u64,
    /// The time from this tick's deadline to the wake.
    pub late: Span,
}

impl Timer {
    /// Starts a grid at `clock.now()` with `rate` ticks per second.
    ///
    /// # Panics
    ///
    /// On a thread that `env` did not start. Make and poll it on one thread.
    #[must_use]
    pub fn new(clock: &Clock, rate: Rate) -> Self {
        let start = clock.now();
        Self {
            clock: clock.clone(),
            rate,
            start,
            next: 0,
            sleep: clock.sleep_until(start),
        }
    }

    /// Waits for the next deadline, then returns its tick. The first call returns at
    /// once. When later deadlines also passed, returns the last one that passed, with
    /// the others in `missed`. Returns `None` at once when `cancel` is cancelled. From
    /// the third call on, a tick allocates nothing.
    ///
    /// # Panics
    ///
    /// When a deadline is more than `i64::MAX` ns (about 292 years) after the start,
    /// or past the end of `Monotonic`.
    pub async fn tick(&mut self, cancel: &cancel::Token) -> Option<Tick> {
        self.sleep.reset(self.start + self.rate.span(self.next));
        cancel.race(&mut self.sleep).await?;
        let now = self.clock.now();
        let n = self.rate.count(now - self.start);
        let tick = Tick {
            n,
            missed: n - self.next,
            late: now - (self.start + self.rate.span(n)),
        };
        self.next = n + 1;
        Some(tick)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::cancel::Token;
    use crate::common::run;

    fn rate(num: u64, den: u64) -> Rate {
        Rate::new(num, den).expect("a valid rate")
    }

    /// Calls `tick` after each stall and returns each tick with its time from the
    /// start.
    fn ticks(rate: Rate, stalls: Vec<Span>) -> Vec<(Tick, Span)> {
        run(move |clock, _, _| async move {
            let token = Token::new();
            let start = clock.now();
            let mut timer = Timer::new(&clock, rate);
            let mut out = Vec::new();
            for stall in stalls {
                let tick = timer.tick(&token).await.expect("a live token");
                out.push((tick, clock.now() - start));
                clock.sleep(stall).await;
            }
            out
        })
    }

    fn tick(n: u64, missed: u64, late: Span) -> Tick {
        Tick { n, missed, late }
    }

    fn ms(n: i64) -> Span {
        Span::from_nanos(n * 1_000_000)
    }

    #[test]
    fn ticks_on_the_grid_without_drift() {
        let span = |nanos| Span::from_nanos(nanos);
        assert_eq!(
            ticks(rate(3, 1), vec![Span::ZERO; 5]),
            [
                (tick(0, 0, Span::ZERO), Span::ZERO),
                (tick(1, 0, Span::ZERO), span(333_333_333)),
                (tick(2, 0, Span::ZERO), span(666_666_666)),
                (tick(3, 0, Span::ZERO), Span::SECOND),
                (tick(4, 0, Span::ZERO), span(1_333_333_333)),
            ]
        );
    }

    #[test]
    fn skips_and_counts_the_ticks_a_stall_missed() {
        assert_eq!(
            ticks(rate(10, 1), vec![ms(350), ms(250), Span::ZERO, Span::ZERO]),
            [
                (tick(0, 0, Span::ZERO), Span::ZERO),
                (tick(3, 2, ms(50)), ms(350)),
                (tick(6, 2, Span::ZERO), ms(600)),
                (tick(7, 0, Span::ZERO), ms(700)),
            ]
        );
    }

    #[test]
    fn returns_none_at_once_when_cancelled_during_the_wait() {
        let (first, second, elapsed) = run(|clock, tasks, _| async move {
            let token = Token::new();
            let canceller = token.clone();
            let sleeper = clock.clone();
            tasks.spawn(async move {
                sleeper.sleep(ms(50)).await;
                canceller.cancel();
            });
            let start = clock.now();
            let mut timer = Timer::new(&clock, rate(1, 1));
            let first = timer.tick(&token).await;
            let second = timer.tick(&token).await;
            (first, second, clock.now() - start)
        });
        assert_eq!(first, Some(tick(0, 0, Span::ZERO)), "tick 0 at once");
        assert_eq!(second, None, "cancelled");
        assert_eq!(elapsed, ms(50), "at the cancel");
    }

    #[test]
    fn returns_none_before_tick_zero_when_already_cancelled() {
        let tick = run(|clock, _, _| async move {
            let token = Token::new();
            token.cancel();
            Timer::new(&clock, rate(1, 1)).tick(&token).await
        });
        assert_eq!(tick, None);
    }

    #[test]
    fn returns_at_once_on_the_grid_when_the_first_call_is_late() {
        let first = run(|clock, _, _| async move {
            let mut timer = Timer::new(&clock, rate(10, 1));
            clock.sleep(ms(250)).await;
            timer.tick(&Token::new()).await
        });
        assert_eq!(first, Some(tick(2, 2, ms(50))));
    }

    #[test]
    fn keeps_the_grid_after_a_cancelled_wait() {
        let (stopped, resumed, elapsed) = run(|clock, tasks, _| async move {
            let token = Token::new();
            let canceller = token.clone();
            let sleeper = clock.clone();
            tasks.spawn(async move {
                sleeper.sleep(ms(50)).await;
                canceller.cancel();
            });
            let start = clock.now();
            let mut timer = Timer::new(&clock, rate(1, 1));
            timer.tick(&token).await.expect("tick 0");
            let stopped = timer.tick(&token).await;
            let resumed = timer.tick(&Token::new()).await;
            (stopped, resumed, clock.now() - start)
        });
        assert_eq!(stopped, None, "cancelled");
        assert_eq!(resumed, Some(tick(1, 0, Span::ZERO)), "tick 1 on the grid");
        assert_eq!(elapsed, Span::SECOND, "at tick 1");
    }

    #[test]
    #[should_panic(expected = "span overflow: 2 samples at 1/5000000000 Hz")]
    fn panics_past_i64_max_nanoseconds_from_the_start() {
        ticks(rate(1, 5_000_000_000), vec![Span::ZERO; 3]);
    }

    proptest! {
        #[test]
        fn keeps_every_tick_on_the_grid(
            num in 1..=1_000_u64,
            den in 1..=10_u64,
            stalls in prop::collection::vec(0..2_000_000_000_i64, 1..16),
        ) {
            let rate = rate(num, den);
            let stalls: Vec<_> = stalls.into_iter().map(Span::from_nanos).collect();
            let mut next = 0;
            let mut call = 0;
            let out = ticks(rate, stalls.clone());
            for ((tick, elapsed), stall) in out.into_iter().zip(stalls) {
                let wake = call.max(rate.span(next).nanos());
                prop_assert_eq!(elapsed.nanos(), wake, "wakes at the next deadline");
                prop_assert!(rate.span(tick.n) <= elapsed, "never early");
                prop_assert!(elapsed < rate.span(tick.n + 1), "the last due tick");
                prop_assert!(tick.n >= next, "never repeats a tick");
                prop_assert_eq!(tick.missed, tick.n - next, "counts the skipped");
                prop_assert_eq!(tick.late.nanos(), wake - rate.span(tick.n).nanos());
                next = tick.n + 1;
                call = wake + stall.nanos();
            }
        }
    }
}
