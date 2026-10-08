//! The timers of an event loop, with the rules of open62541's timer policies. Times
//! are 100 ns ticks of a monotonic clock, as `UA_DateTime` counts them.

use std::collections::{BTreeMap, BTreeSet};

/// What a repeated timer does after it misses a cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Policy {
    /// Fires once, then goes away.
    Once,
    /// Fires at once, then again one interval after the time of that run.
    CurrentTime,
    /// Fires at once, then again on the next cycle counted from its base time.
    BaseTime,
}

/// The time between two runs of a timer: at least 10 µs, as open62541 requires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Interval(i64);

impl Interval {
    /// Gives the interval of `millis` milliseconds, or `None` when it is below 10 µs,
    /// above `i64::MAX` ticks, or not a number.
    pub(crate) fn from_millis(millis: f64) -> Option<Self> {
        // 2^63, the first float past `i64::MAX`.
        const LIMIT: f64 = 9_223_372_036_854_775_808.0;
        let ticks = millis * 10_000.0;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the check keeps the value inside i64"
        )]
        (100.0..LIMIT)
            .contains(&ticks)
            .then_some(Self(ticks as i64))
    }
}

/// Timers that each hold an item of type `T`, by key. Keys start at 1 and are never
/// used again.
#[derive(Debug)]
pub(crate) struct Timers<T> {
    entries: BTreeMap<u64, Entry<T>>,
    /// Each entry by (due time, key), so equal due times fire in the order added.
    order: BTreeSet<(i64, u64)>,
    last: u64,
}

#[derive(Debug)]
struct Entry<T> {
    due: i64,
    interval: Interval,
    policy: Policy,
    item: T,
}

impl<T: Copy> Timers<T> {
    /// Makes a set with no timers.
    pub(crate) fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
            order: BTreeSet::new(),
            last: 0,
        }
    }

    /// Adds a timer and gives its key. It first fires one interval after `now`, or,
    /// with a `base`, at the first cycle counted from `base` after `now`.
    pub(crate) fn add(
        &mut self,
        now: i64,
        interval: Interval,
        base: Option<i64>,
        policy: Policy,
        item: T,
    ) -> u64 {
        self.last += 1;
        let due = first(now, interval, base);
        self.order.insert((due, self.last));
        let entry = Entry {
            due,
            interval,
            policy,
            item,
        };
        self.entries.insert(self.last, entry);
        self.last
    }

    /// Gives the timer of `key` a new interval, base, and policy, and times its next
    /// run as [`Timers::add`] does. Gives `false` when no timer has `key`.
    pub(crate) fn modify(
        &mut self,
        key: u64,
        now: i64,
        interval: Interval,
        base: Option<i64>,
        policy: Policy,
    ) -> bool {
        let Some(entry) = self.entries.get_mut(&key) else {
            return false;
        };
        self.order.remove(&(entry.due, key));
        entry.due = first(now, interval, base);
        entry.interval = interval;
        entry.policy = policy;
        self.order.insert((entry.due, key));
        true
    }

    /// Removes the timer of `key`, if there is one.
    pub(crate) fn remove(&mut self, key: u64) {
        if let Some(entry) = self.entries.remove(&key) {
            self.order.remove(&(entry.due, key));
        }
    }

    /// The earliest due time, or `None` with no timers.
    pub(crate) fn next(&self) -> Option<i64> {
        self.order.first().map(|&(due, _)| due)
    }

    /// Takes the item of the earliest timer due at or before `now`, and removes or
    /// times again that timer by its policy. Its next run is always after `now`, so
    /// a loop of calls with one `now` ends.
    #[expect(
        clippy::unwrap_in_result,
        reason = "a key in the order with no entry is a defect, not an empty set"
    )]
    pub(crate) fn pop(&mut self, now: i64) -> Option<T> {
        let &(due, key) = self.order.first().filter(|&&(due, _)| due <= now)?;
        self.order.remove(&(due, key));
        let entry = self
            .entries
            .get_mut(&key)
            .expect("invariant: each key in the order has an entry");
        let item = entry.item;
        let Interval(interval) = entry.interval;
        let next = due.saturating_add(interval);
        entry.due = match entry.policy {
            Policy::Once => {
                self.entries.remove(&key);
                return Some(item);
            }
            _ if next > now => next,
            Policy::CurrentTime => now.saturating_add(interval),
            Policy::BaseTime => after(now, entry.interval, due),
        };
        self.order.insert((entry.due, key));
        Some(item)
    }
}

/// The first run of a timer added at `now`.
fn first(now: i64, interval: Interval, base: Option<i64>) -> i64 {
    match base {
        None => now.saturating_add(interval.0),
        Some(base) => after(now, interval, base),
    }
}

/// The first time after `now` that is a whole count of intervals from `base`.
fn after(now: i64, Interval(interval): Interval, base: i64) -> i64 {
    let into = (i128::from(now) - i128::from(base)).rem_euclid(i128::from(interval));
    let next = i128::from(now) + i128::from(interval) - into;
    i64::try_from(next).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests;
