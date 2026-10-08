use proptest::prelude::*;

use super::{Interval, Policy, Timers};

fn ticks(ticks: i64) -> Interval {
    Interval(ticks)
}

/// One timer of the model: the next due time found by stepping, not by a modulo.
#[derive(Clone, Debug)]
struct Model {
    key: u64,
    due: i64,
    interval: i64,
    policy: Policy,
}

impl Model {
    fn new(
        key: u64,
        now: i64,
        interval: i64,
        base: Option<i64>,
        policy: Policy,
    ) -> Self {
        let due = base.map_or(now + interval, |base| step_past(base, interval, now));
        Self {
            key,
            due,
            interval,
            policy,
        }
    }

    /// Fires once at `now`, and gives `false` when the timer goes away.
    fn fire(&mut self, now: i64) -> bool {
        self.due = match self.policy {
            Policy::Once => return false,
            _ if self.due + self.interval > now => self.due + self.interval,
            Policy::CurrentTime => now + self.interval,
            Policy::BaseTime => step_past(self.due, self.interval, now),
        };
        true
    }
}

/// The first of `from + k * interval`, for any integer `k`, after `now`.
fn step_past(mut from: i64, interval: i64, now: i64) -> i64 {
    while from > now {
        from -= interval;
    }
    while from <= now {
        from += interval;
    }
    from
}

fn policy() -> impl Strategy<Value = Policy> {
    prop_oneof![
        Just(Policy::Once),
        Just(Policy::CurrentTime),
        Just(Policy::BaseTime)
    ]
}

proptest! {
    #[test]
    fn pops_each_due_timer_once_in_due_order_by_its_policy(
        added in prop::collection::vec(
            (100_i64..1_000, prop::option::of(-5_000_i64..5_000), policy()),
            1..8,
        ),
        steps in prop::collection::vec(0_i64..3_000, 1..20),
        removed in prop::collection::vec(any::<prop::sample::Index>(), 0..3),
    ) {
        let mut timers = Timers::new();
        let mut model = Vec::new();
        for (interval, base, policy) in added {
            let key = timers.add(0, ticks(interval), base, policy, model.len());
            model.push(Some(Model::new(key, 0, interval, base, policy)));
        }
        for index in removed {
            let at = index.index(model.len());
            if let Some(timer) = model[at].take() {
                timers.remove(timer.key);
            }
        }
        let mut now = 0;
        for step in steps {
            now += step;
            let mut due: Vec<(i64, u64, usize)> = (model.iter().enumerate())
                .filter_map(|(at, timer)| Some((timer.as_ref()?, at)))
                .filter(|(timer, _)| timer.due <= now)
                .map(|(timer, at)| (timer.due, timer.key, at))
                .collect();
            due.sort_unstable();
            let expected: Vec<usize> = due.iter().map(|&(_, _, at)| at).collect();
            for &(_, _, at) in &due {
                let kept = model[at].as_mut().is_some_and(|timer| timer.fire(now));
                if !kept {
                    model[at] = None;
                }
            }
            let popped: Vec<usize> = std::iter::from_fn(|| timers.pop(now)).collect();
            prop_assert_eq!(popped, expected);
            let next = model.iter().flatten().map(|timer| timer.due).min();
            prop_assert_eq!(timers.next(), next);
        }
    }
}

#[test]
fn keys_start_at_1_and_are_not_used_again() {
    let mut timers = Timers::new();
    let first = timers.add(0, ticks(100), None, Policy::Once, ());
    timers.remove(first);
    let second = timers.add(0, ticks(100), None, Policy::Once, ());
    assert_eq!((first, second), (1, 2));
}

#[test]
fn modify_times_the_next_run_from_now_and_refuses_an_unknown_key() {
    let mut timers = Timers::new();
    let key = timers.add(0, ticks(1_000), None, Policy::CurrentTime, 'a');
    assert!(timers.modify(key, 500, ticks(200), None, Policy::Once));
    assert_eq!(timers.next(), Some(700));
    assert!(timers.modify(key, 500, ticks(200), Some(450), Policy::BaseTime));
    assert_eq!(timers.next(), Some(650));
    assert!(!timers.modify(key + 1, 500, ticks(200), None, Policy::Once));
    assert_eq!((timers.pop(650), timers.next()), (Some('a'), Some(850)));
}

#[test]
fn remove_of_an_unknown_key_keeps_the_others() {
    let mut timers = Timers::new();
    timers.add(0, ticks(100), None, Policy::Once, 'a');
    timers.remove(2);
    assert_eq!((timers.pop(100), timers.next()), (Some('a'), None));
}

#[test]
fn from_millis_takes_10_us_to_i64_max_ticks() {
    assert_eq!(Interval::from_millis(0.01), Some(ticks(100)));
    assert_eq!(Interval::from_millis(1_000.0), Some(ticks(10_000_000)));
    assert_eq!(
        Interval::from_millis(9.2e14),
        Some(ticks(9_200_000_000_000_000_000))
    );
    for refused in [0.0099, 0.0, -1.0, 9.3e14, f64::NAN, f64::INFINITY] {
        assert_eq!(Interval::from_millis(refused), None, "{refused}");
    }
}
