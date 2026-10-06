use std::fmt;

use types::time::Monotonic;

use crate::{Error, Handoff, Lease, Writer};

/// A writer's place in one [`Gate`]. Keys order by when the writer opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Key(u64);

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Decides which open writer holds control of one index.
///
/// The writer with the highest authority holds control. A writer with equal or lower
/// authority waits, and a tie keeps the holder. When the holder closes or its control
/// lease runs out, the waiter with the highest authority takes control, and on a tie
/// the one that opened first.
#[derive(Debug, Default)]
pub struct Gate {
    /// Sorted by key, for `position`.
    claims: Vec<Claim>,
    seat: Seat,
    next: u64,
    /// The holder named by the newest handoff record in the index log.
    record: Option<Record>,
    /// The holder differs from the record. Kept at each change of seat, so that a
    /// frame does not compare subjects.
    unrecorded: bool,
    /// The writer and time of the newest check that passed on this seat.
    checked: Option<(Key, Monotonic)>,
}

/// A write that passed [`Gate::check`]. [`Gate::renew`] spends it.
#[must_use]
#[derive(Debug)]
pub struct Permit {
    key: Key,
    at: Monotonic,
}

#[derive(Debug)]
enum Record {
    /// The open writer with this key.
    Open(Key),
    /// A writer with no open claim: it closed after the record, or the gate
    /// recovered it.
    Closed(Writer),
}

#[derive(Debug)]
struct Claim {
    key: Key,
    writer: Writer,
    lease: Option<Lease>,
    expired: bool,
}

#[derive(Debug, Default)]
enum Seat {
    #[default]
    Empty,
    Held {
        key: Key,
        expiry: Option<Expiry>,
    },
    /// The holder from the index log after a restart, not yet reopened.
    Recovered {
        writer: Writer,
        until: Monotonic,
    },
}

#[derive(Debug)]
struct Expiry {
    lease: Lease,
    at: Monotonic,
}

impl Gate {
    /// An empty gate, for an index with no handoff records.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A gate for an index whose last handoff record names `last`, after a restart or
    /// a failover. For one control lease `grace` from `now`, `last` holds control
    /// without a connected writer: no write passes, and only a writer of the same
    /// subject or one with higher authority takes control.
    #[must_use]
    pub fn recover(last: Writer, now: Monotonic, grace: Lease) -> Self {
        Self {
            seat: Seat::Recovered {
                writer: last.clone(),
                until: deadline(now, grace),
            },
            record: Some(Record::Closed(last)),
            ..Self::default()
        }
    }

    /// Opens a writer at `now`. It takes control when the gate is empty, when its
    /// authority is higher than the holder's, or when it reopens the subject of a
    /// recovered holder. Otherwise it waits.
    pub fn open(
        &mut self,
        writer: Writer,
        lease: Option<Lease>,
        now: Monotonic,
    ) -> Key {
        self.advance(now);
        let key = Key(self.next);
        self.next += 1;
        let authority = writer.authority;
        let reopened = matches!(
            &self.seat,
            Seat::Recovered { writer: last, .. } if last.subject == writer.subject
        );
        let outranks = self.holder().is_none_or(|h| authority > h.authority);
        self.claims.push(Claim {
            key,
            writer,
            lease,
            expired: false,
        });
        if reopened || outranks {
            self.take(key, now);
        }
        if reopened
            && let Some(best) = self.best()
            && self.claim(best).writer.authority > authority
        {
            self.take(best, now);
        }
        key
    }

    /// Closes the writer `key` at `now`. When it held control, the waiter with the
    /// highest authority takes control.
    ///
    /// # Panics
    ///
    /// When `key` is not open in this gate.
    pub fn close(&mut self, key: Key, now: Monotonic) {
        self.advance(now);
        let i = self.position(key);
        let claim = self.claims.remove(i);
        if matches!(self.record, Some(Record::Open(open)) if open == key) {
            self.record = Some(Record::Closed(claim.writer));
        }
        if matches!(self.seat, Seat::Held { key: held, .. } if held == key) {
            self.elect(now);
        }
    }

    /// Checks a write from `key` at `now`. It applies what came due by `now`, as
    /// [`Gate::advance`] does, and does not renew the control lease. For n open
    /// writers, takes O(1) time for a write from the holder when nothing came due,
    /// O(log n) to refuse a write, and O(n) when control changes. Does not allocate.
    ///
    /// # Errors
    ///
    /// [`Error::Waiting`] when another writer holds control, [`Error::Reserved`]
    /// while a recovered holder may still reopen, and [`Error::Expired`] when the
    /// control lease of `key` ran out.
    ///
    /// # Panics
    ///
    /// When `key` is not open in this gate.
    pub fn check(&mut self, key: Key, now: Monotonic) -> Result<Permit, Error> {
        self.advance(now);
        if matches!(self.seat, Seat::Held { key: held, .. } if held == key) {
            self.checked = Some((key, now));
            return Ok(Permit { key, at: now });
        }
        if self.claim(key).expired {
            Err(Error::Expired)
        } else if let Seat::Recovered { .. } = self.seat {
            Err(Error::Reserved)
        } else {
            Err(Error::Waiting)
        }
    }

    /// Renews the holder's control lease from the time of `permit`. Call it when the
    /// caller accepts the write, before the gate checks another write. `permit` must
    /// come from [`Gate::check`] on this gate. Takes O(1) time and does not allocate.
    ///
    /// # Panics
    ///
    /// When the gate checked another write or changed holder after the check of
    /// `permit`.
    #[expect(clippy::needless_pass_by_value, reason = "a permit is spent once")]
    pub fn renew(&mut self, permit: Permit) {
        let Permit { key, at } = permit;
        assert!(
            self.checked == Some((key, at)),
            "invariant: the gate changed after the check of writer {key}"
        );
        if let Seat::Held {
            expiry: Some(expiry),
            ..
        } = &mut self.seat
        {
            expiry.at = deadline(at, expiry.lease);
        }
    }

    /// Applies what has come due by `now`: a holder whose control lease ran out loses
    /// control, and a recovered holder that did not reopen in time is dropped. Call it
    /// at [`Gate::deadline`].
    pub fn advance(&mut self, now: Monotonic) {
        if self.deadline().is_none_or(|due| now < due) {
            return;
        }
        if let Seat::Held { key, .. } = self.seat {
            let i = self.position(key);
            self.claims[i].expired = true;
        }
        self.elect(now);
    }

    /// When the gate next changes on its own, or `None` when only an input can change
    /// it.
    #[must_use]
    pub fn deadline(&self) -> Option<Monotonic> {
        match &self.seat {
            Seat::Empty | Seat::Held { expiry: None, .. } => None,
            Seat::Held {
                expiry: Some(expiry),
                ..
            } => Some(expiry.at),
            Seat::Recovered { until, .. } => Some(*until),
        }
    }

    /// The writer that holds control, or `None` when the gate is empty.
    #[must_use]
    pub fn holder(&self) -> Option<&Writer> {
        match &self.seat {
            Seat::Empty => None,
            Seat::Held { key, .. } => Some(&self.claim(*key).writer),
            Seat::Recovered { writer, .. } => Some(writer),
        }
    }

    /// The holder, when its subject or authority differs from the holder recorded
    /// last by [`Gate::recorded`] or [`Gate::recover`]. A new gate starts with no
    /// holder recorded. For n open writers, takes O(1) time when it returns `None`
    /// and O(log n) when it returns `Some`. Does not allocate.
    #[must_use]
    pub fn handoff(&self) -> Option<Handoff<'_>> {
        self.unrecorded.then(|| Handoff { to: self.holder() })
    }

    /// Marks the holder as the newest record in the index log. Call it after the
    /// caller records [`Gate::handoff`] and before the next input to the gate. Does
    /// not allocate.
    pub fn recorded(&mut self) {
        if self.unrecorded {
            match self.seat {
                Seat::Empty => self.record = None,
                Seat::Held { key, .. } => self.record = Some(Record::Open(key)),
                Seat::Recovered { .. } => {
                    unreachable!("invariant: a recovered seat is never unrecorded")
                }
            }
            self.unrecorded = false;
        }
    }

    fn take(&mut self, key: Key, now: Monotonic) {
        let expiry = self.claim(key).lease.map(|lease| Expiry {
            lease,
            at: deadline(now, lease),
        });
        self.sit(Seat::Held { key, expiry });
    }

    fn elect(&mut self, now: Monotonic) {
        if let Some(key) = self.best() {
            self.take(key, now);
        } else {
            self.sit(Seat::Empty);
        }
    }

    fn sit(&mut self, seat: Seat) {
        self.seat = seat;
        self.checked = None;
        self.unrecorded = self.holder() != self.last();
    }

    /// The holder named by the newest handoff record.
    fn last(&self) -> Option<&Writer> {
        match self.record.as_ref()? {
            Record::Open(key) => Some(&self.claim(*key).writer),
            Record::Closed(writer) => Some(writer),
        }
    }

    /// The waiting claim with the highest authority, and on a tie the first opened.
    fn best(&self) -> Option<Key> {
        let mut best: Option<&Claim> = None;
        for claim in self.claims.iter().filter(|c| !c.expired) {
            if best.is_none_or(|b| claim.writer.authority > b.writer.authority) {
                best = Some(claim);
            }
        }
        best.map(|c| c.key)
    }

    fn claim(&self, key: Key) -> &Claim {
        &self.claims[self.position(key)]
    }

    fn position(&self, key: Key) -> usize {
        self.claims
            .binary_search_by_key(&key, |c| c.key)
            .unwrap_or_else(|_| panic!("writer {key} is not open in this gate"))
    }
}

/// The end of a control lease taken or renewed at `now`. A lease that would end past
/// the last monotonic reading ends at that reading.
fn deadline(now: Monotonic, lease: Lease) -> Monotonic {
    now.checked_add(lease.span()).unwrap_or(Monotonic(u64::MAX))
}

#[cfg(test)]
mod tests {
    use types::authority::Authority;
    use types::time::Span;

    use super::*;

    fn writer(subject: &str, authority: u8) -> Writer {
        Writer {
            subject: subject.parse().expect("valid name"),
            authority: Authority(authority),
        }
    }

    fn at(nanos: u64) -> Monotonic {
        Monotonic(nanos)
    }

    fn lease(nanos: i64) -> Lease {
        Lease::new(Span::from_nanos(nanos)).expect("positive lease")
    }

    fn to(writer: &Writer) -> Handoff<'_> {
        Handoff { to: Some(writer) }
    }

    const EMPTY: Option<Handoff<'_>> = Some(Handoff { to: None });

    /// Checks a write and renews the lease, as the caller does for a write it accepts.
    fn write(gate: &mut Gate, key: Key, now: Monotonic) -> Result<(), Error> {
        gate.check(key, now).map(|permit| gate.renew(permit))
    }

    mod open {
        use super::*;

        #[test]
        fn takes_control_of_an_empty_gate() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 0), None, at(0));
            assert_eq!(gate.handoff(), Some(to(&writer("a", 0))));
            assert_eq!(write(&mut gate, a, at(1)), Ok(()));
        }

        #[test]
        fn higher_authority_takes_control() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 100), None, at(0));
            gate.recorded();
            let b = gate.open(writer("b", 101), None, at(1));
            assert_eq!(gate.handoff(), Some(to(&writer("b", 101))));
            assert_eq!(write(&mut gate, a, at(2)), Err(Error::Waiting));
            assert_eq!(write(&mut gate, b, at(2)), Ok(()));
        }

        #[test]
        fn equal_authority_waits() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 100), None, at(0));
            gate.recorded();
            let b = gate.open(writer("b", 100), None, at(1));
            assert_eq!(gate.handoff(), None);
            assert_eq!(write(&mut gate, a, at(2)), Ok(()));
            assert_eq!(write(&mut gate, b, at(2)), Err(Error::Waiting));
        }

        #[test]
        fn lower_authority_waits() {
            let mut gate = Gate::new();
            gate.open(writer("a", 100), None, at(0));
            gate.recorded();
            let b = gate.open(writer("b", 99), None, at(1));
            assert_eq!(gate.handoff(), None);
            assert_eq!(write(&mut gate, b, at(2)), Err(Error::Waiting));
        }

        #[test]
        fn absolute_authority_is_never_taken() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 255), None, at(0));
            gate.recorded();
            gate.open(writer("b", 255), None, at(1));
            assert_eq!(gate.handoff(), None);
            assert_eq!(gate.holder(), Some(&writer("a", 255)));
            assert_eq!(write(&mut gate, a, at(2)), Ok(()));
        }

        #[test]
        fn absolute_authority_takes_from_lower() {
            let mut gate = Gate::new();
            gate.open(writer("a", 254), None, at(0));
            gate.recorded();
            gate.open(writer("b", 255), None, at(1));
            assert_eq!(gate.handoff(), Some(to(&writer("b", 255))));
        }
    }

    mod close {
        use super::*;

        #[test]
        fn hands_control_to_the_highest_waiter() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 200), None, at(0));
            gate.open(writer("b", 100), None, at(1));
            let c = gate.open(writer("c", 150), None, at(2));
            gate.recorded();
            gate.close(a, at(3));
            assert_eq!(gate.handoff(), Some(to(&writer("c", 150))));
            assert_eq!(write(&mut gate, c, at(4)), Ok(()));
        }

        #[test]
        fn breaks_a_tie_by_open_order() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 200), None, at(0));
            let b = gate.open(writer("b", 100), None, at(1));
            gate.open(writer("c", 100), None, at(2));
            gate.recorded();
            gate.close(a, at(3));
            assert_eq!(gate.handoff(), Some(to(&writer("b", 100))));
            assert_eq!(write(&mut gate, b, at(4)), Ok(()));
        }

        #[test]
        fn empties_the_gate_after_the_last_writer() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 0), None, at(0));
            gate.recorded();
            gate.close(a, at(1));
            assert_eq!(gate.handoff(), EMPTY);
            assert_eq!(gate.holder(), None);
        }

        #[test]
        fn of_a_waiter_changes_nothing() {
            let mut gate = Gate::new();
            gate.open(writer("a", 100), None, at(0));
            let b = gate.open(writer("b", 100), None, at(1));
            gate.recorded();
            gate.close(b, at(2));
            assert_eq!(gate.handoff(), None);
            assert_eq!(gate.holder(), Some(&writer("a", 100)));
        }

        #[test]
        #[should_panic(expected = "writer 0 is not open in this gate")]
        fn twice_panics() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 0), None, at(0));
            gate.close(a, at(1));
            gate.close(a, at(2));
        }
    }

    mod lease {
        use super::*;

        #[test]
        fn runs_out_at_the_deadline() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 100), Some(lease(10)), at(0));
            gate.recorded();
            assert_eq!(gate.deadline(), Some(at(10)));
            gate.advance(at(9));
            assert_eq!(gate.handoff(), None);
            gate.advance(at(10));
            assert_eq!(gate.handoff(), EMPTY);
            assert_eq!(write(&mut gate, a, at(11)), Err(Error::Expired));
        }

        #[test]
        fn is_not_renewed_by_a_check() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 100), Some(lease(10)), at(0));
            let permit = gate.check(a, at(8)).expect("a holds control");
            assert_eq!(gate.deadline(), Some(at(10)));
            gate.renew(permit);
            assert_eq!(gate.deadline(), Some(at(18)));
        }

        #[test]
        #[should_panic(
            expected = "invariant: the gate changed after the check of writer 0"
        )]
        fn panics_when_renewed_after_the_holder_changed() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 100), Some(lease(10)), at(0));
            let permit = gate.check(a, at(1)).expect("a holds control");
            gate.open(writer("b", 200), None, at(1));
            gate.renew(permit);
        }

        #[test]
        #[should_panic(
            expected = "invariant: the gate changed after the check of writer 0"
        )]
        fn panics_when_renewed_after_a_handoff_and_back() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 100), Some(lease(10)), at(0));
            let permit = gate.check(a, at(1)).expect("a holds control");
            let b = gate.open(writer("b", 200), None, at(5));
            gate.close(b, at(8));
            gate.renew(permit);
        }

        #[test]
        #[should_panic(
            expected = "invariant: the gate changed after the check of writer 0"
        )]
        fn panics_when_an_older_permit_is_renewed() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 100), Some(lease(10)), at(0));
            let first = gate.check(a, at(1)).expect("a holds control");
            let second = gate.check(a, at(9)).expect("a holds control");
            gate.renew(second);
            gate.renew(first);
        }

        #[test]
        #[should_panic(
            expected = "invariant: the gate changed after the check of writer 0"
        )]
        fn panics_when_renewed_after_the_holder_closed() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 100), Some(lease(10)), at(0));
            let permit = gate.check(a, at(1)).expect("a holds control");
            gate.close(a, at(1));
            gate.renew(permit);
        }

        #[test]
        fn is_renewed_by_each_write() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 100), Some(lease(10)), at(0));
            assert_eq!(write(&mut gate, a, at(8)), Ok(()));
            assert_eq!(gate.deadline(), Some(at(18)));
            assert_eq!(write(&mut gate, a, at(17)), Ok(()));
            assert_eq!(gate.deadline(), Some(at(27)));
        }

        #[test]
        fn hands_control_to_the_next_waiter() {
            let mut gate = Gate::new();
            gate.open(writer("a", 200), Some(lease(10)), at(0));
            let b = gate.open(writer("b", 100), Some(lease(5)), at(1));
            gate.recorded();
            gate.advance(at(12));
            assert_eq!(gate.handoff(), Some(to(&writer("b", 100))));
            assert_eq!(gate.deadline(), Some(at(17)));
            assert_eq!(write(&mut gate, b, at(13)), Ok(()));
        }

        #[test]
        fn keeps_an_expired_writer_out_until_it_reopens() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 200), Some(lease(10)), at(0));
            gate.open(writer("b", 100), None, at(1));
            gate.advance(at(10));
            gate.recorded();
            assert_eq!(write(&mut gate, a, at(11)), Err(Error::Expired));
            assert_eq!(gate.holder(), Some(&writer("b", 100)));
            gate.close(a, at(12));
            gate.open(writer("a", 200), Some(lease(10)), at(13));
            assert_eq!(gate.handoff(), Some(to(&writer("a", 200))));
        }

        #[test]
        fn is_applied_by_a_write_past_the_deadline() {
            let mut gate = Gate::new();
            gate.open(writer("a", 200), Some(lease(10)), at(0));
            let b = gate.open(writer("b", 100), None, at(1));
            gate.recorded();
            assert_eq!(write(&mut gate, b, at(10)), Ok(()));
            assert_eq!(gate.handoff(), Some(to(&writer("b", 100))));
        }

        #[test]
        fn is_not_needed_for_control() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 0), None, at(0));
            assert_eq!(gate.deadline(), None);
            assert_eq!(write(&mut gate, a, at(u64::MAX)), Ok(()));
        }

        #[test]
        fn ends_at_the_last_monotonic_reading() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 0), Some(lease(i64::MAX)), at(u64::MAX - 1));
            assert_eq!(gate.deadline(), Some(at(u64::MAX)));
            assert_eq!(write(&mut gate, a, at(u64::MAX - 1)), Ok(()));
            assert_eq!(write(&mut gate, a, at(u64::MAX)), Err(Error::Expired));
        }

        #[test]
        fn is_not_renewed_by_inputs_from_waiters() {
            let mut gate = Gate::new();
            gate.open(writer("a", 100), Some(lease(10)), at(0));
            let b = gate.open(writer("b", 100), None, at(1));
            let c = gate.open(writer("c", 50), None, at(2));
            assert_eq!(write(&mut gate, b, at(3)), Err(Error::Waiting));
            gate.close(c, at(4));
            gate.advance(at(5));
            assert_eq!(gate.deadline(), Some(at(10)));
        }
    }

    mod recover {
        use super::*;

        fn recovered() -> Gate {
            Gate::recover(writer("a", 100), at(0), lease(10))
        }

        #[test]
        fn holds_without_a_writer_until_the_grace_ends() {
            let mut gate = recovered();
            assert_eq!(gate.holder(), Some(&writer("a", 100)));
            assert_eq!(gate.deadline(), Some(at(10)));
            assert_eq!(gate.handoff(), None);
            gate.advance(at(10));
            assert_eq!(gate.handoff(), EMPTY);
        }

        #[test]
        fn gives_control_back_to_the_same_subject() {
            let mut gate = recovered();
            let a = gate.open(writer("a", 100), None, at(5));
            assert_eq!(gate.handoff(), None);
            assert_eq!(gate.deadline(), None);
            assert_eq!(write(&mut gate, a, at(6)), Ok(()));
        }

        #[test]
        fn keeps_the_place_of_a_subject_that_reopens_at_equal_authority() {
            let mut gate = recovered();
            gate.open(writer("b", 100), None, at(1));
            let a = gate.open(writer("a", 100), None, at(2));
            assert_eq!(gate.handoff(), None);
            assert_eq!(write(&mut gate, a, at(3)), Ok(()));
        }

        #[test]
        fn yields_a_subject_that_reopens_lower_to_a_higher_waiter() {
            let mut gate = recovered();
            let b = gate.open(writer("b", 90), None, at(1));
            gate.open(writer("a", 80), None, at(2));
            assert_eq!(gate.handoff(), Some(to(&writer("b", 90))));
            assert_eq!(write(&mut gate, b, at(3)), Ok(()));
        }

        #[test]
        fn makes_equal_authority_wait() {
            let mut gate = recovered();
            let b = gate.open(writer("b", 100), None, at(1));
            assert_eq!(gate.handoff(), None);
            assert_eq!(write(&mut gate, b, at(2)), Err(Error::Reserved));
        }

        #[test]
        fn yields_to_higher_authority() {
            let mut gate = recovered();
            let b = gate.open(writer("b", 101), None, at(1));
            assert_eq!(gate.handoff(), Some(to(&writer("b", 101))));
            assert_eq!(write(&mut gate, b, at(2)), Ok(()));
        }

        #[test]
        fn hands_control_to_a_waiter_when_the_grace_ends() {
            let mut gate = recovered();
            let b = gate.open(writer("b", 100), None, at(1));
            assert_eq!(write(&mut gate, b, at(10)), Ok(()));
            assert_eq!(gate.handoff(), Some(to(&writer("b", 100))));
        }
    }

    mod handoff {
        use super::*;

        #[test]
        fn is_none_when_the_value_is_the_same() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 100), None, at(0));
            gate.open(writer("a", 100), None, at(1));
            gate.recorded();
            gate.close(a, at(2));
            assert_eq!(gate.handoff(), None);
        }

        #[test]
        fn coalesces_two_changes_from_one_input() {
            let mut gate = Gate::new();
            gate.open(writer("a", 200), Some(lease(10)), at(0));
            gate.open(writer("b", 100), None, at(1));
            gate.recorded();
            gate.open(writer("c", 150), None, at(10));
            assert_eq!(gate.handoff(), Some(to(&writer("c", 150))));
        }

        #[test]
        fn is_kept_until_recorded() {
            let mut gate = Gate::new();
            gate.open(writer("a", 0), None, at(0));
            assert_eq!(gate.handoff(), Some(to(&writer("a", 0))));
            assert_eq!(gate.handoff(), Some(to(&writer("a", 0))));
            gate.recorded();
            assert_eq!(gate.handoff(), None);
        }

        #[test]
        fn is_none_when_the_recorded_holder_is_back() {
            let mut gate = Gate::new();
            gate.open(writer("a", 100), None, at(0));
            gate.recorded();
            let b = gate.open(writer("b", 200), None, at(1));
            assert_eq!(gate.handoff(), Some(to(&writer("b", 200))));
            gate.close(b, at(2));
            assert_eq!(gate.handoff(), None);
        }

        #[test]
        fn compares_with_a_recorded_holder_that_closed() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 100), None, at(0));
            gate.recorded();
            gate.close(a, at(1));
            assert_eq!(gate.handoff(), EMPTY);
            gate.open(writer("a", 100), None, at(2));
            assert_eq!(gate.handoff(), None);
        }

        #[test]
        fn is_found_after_the_recorded_holder_closed() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 100), None, at(0));
            gate.recorded();
            gate.close(a, at(1));
            gate.open(writer("a", 101), None, at(2));
            assert_eq!(gate.handoff(), Some(to(&writer("a", 101))));
        }

        #[test]
        fn is_found_when_a_writer_opens_after_a_recorded_empty_gate() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 100), None, at(0));
            gate.recorded();
            gate.close(a, at(1));
            gate.recorded();
            gate.open(writer("a", 100), None, at(2));
            assert_eq!(gate.handoff(), Some(to(&writer("a", 100))));
        }

        #[test]
        fn compares_with_a_recovered_holder_that_was_replaced() {
            let mut gate = Gate::recover(writer("a", 100), at(0), lease(10));
            let b = gate.open(writer("b", 101), None, at(1));
            gate.close(b, at(2));
            assert_eq!(gate.handoff(), EMPTY);
            gate.open(writer("a", 100), None, at(3));
            assert_eq!(gate.handoff(), None);
        }

        #[test]
        fn compares_with_a_recovered_holder_after_its_grace() {
            let mut gate = Gate::recover(writer("a", 100), at(0), lease(10));
            gate.advance(at(10));
            assert_eq!(gate.handoff(), EMPTY);
            gate.open(writer("a", 100), None, at(11));
            assert_eq!(gate.handoff(), None);
        }

        #[test]
        fn is_none_when_a_new_gate_is_empty_again() {
            let mut gate = Gate::new();
            let a = gate.open(writer("a", 100), None, at(0));
            gate.close(a, at(1));
            assert_eq!(gate.handoff(), None);
        }
    }

    mod properties {
        use std::cmp::Reverse;

        use proptest::prelude::*;

        use super::*;

        #[derive(Clone, Debug)]
        enum Input {
            Open {
                subject: usize,
                authority: u8,
                lease: Option<i64>,
            },
            Close(usize),
            /// A write the caller refuses after the check: no renewal.
            Check(usize),
            Write(usize),
            Record,
            Advance,
        }

        const SUBJECTS: [&str; 3] = ["a", "b", "c"];

        /// The gate rules stated a second way: open claims in open order, election
        /// by sorting, and expired claims kept apart.
        #[derive(Default)]
        struct Model {
            open: Vec<(Key, Writer, Option<Lease>)>,
            expired: Vec<Key>,
            holder: Option<(Key, Option<Monotonic>)>,
            recovered: Option<(Writer, Monotonic)>,
        }

        impl Model {
            fn due(&mut self, now: Monotonic) {
                if self
                    .recovered
                    .as_ref()
                    .is_some_and(|(_, until)| *until <= now)
                {
                    self.recovered = None;
                    self.elect(now);
                }
                if let Some((key, Some(end))) = self.holder
                    && end <= now
                {
                    self.open.retain(|(k, ..)| *k != key);
                    self.expired.push(key);
                    self.elect(now);
                }
            }

            fn elect(&mut self, now: Monotonic) {
                self.holder = None;
                let best = self
                    .open
                    .iter()
                    .min_by_key(|(key, writer, _)| (Reverse(writer.authority), *key))
                    .map(|(key, ..)| *key);
                if let Some(key) = best {
                    self.seat(key, now);
                }
            }

            fn seat(&mut self, key: Key, now: Monotonic) {
                let lease = self.find(key).2;
                self.holder = Some((key, lease.map(|lease| now + lease.span())));
            }

            fn find(&self, key: Key) -> &(Key, Writer, Option<Lease>) {
                self.open
                    .iter()
                    .find(|(k, ..)| *k == key)
                    .expect("open in the model")
            }

            fn holder(&self) -> Option<&Writer> {
                match (&self.recovered, self.holder) {
                    (Some((writer, _)), _) => Some(writer),
                    (None, Some((key, _))) => Some(&self.find(key).1),
                    (None, None) => None,
                }
            }

            fn deadline(&self) -> Option<Monotonic> {
                match (&self.recovered, self.holder) {
                    (Some((_, until)), _) => Some(*until),
                    (None, Some((_, end))) => end,
                    (None, None) => None,
                }
            }

            fn open(
                &mut self,
                key: Key,
                writer: Writer,
                lease: Option<Lease>,
                now: Monotonic,
            ) {
                self.due(now);
                let authority = writer.authority;
                let reopens = self
                    .recovered
                    .as_ref()
                    .is_some_and(|(last, _)| last.subject == writer.subject);
                let outranks = self.holder().is_none_or(|h| authority > h.authority);
                self.open.push((key, writer, lease));
                if reopens || outranks {
                    self.recovered = None;
                    self.seat(key, now);
                }
                if reopens && self.open.iter().any(|(_, w, _)| w.authority > authority)
                {
                    self.elect(now);
                }
            }

            fn close(&mut self, key: Key, now: Monotonic) {
                self.due(now);
                if let Some(i) = self.expired.iter().position(|k| *k == key) {
                    self.expired.remove(i);
                    return;
                }
                self.open.retain(|(k, ..)| *k != key);
                if self.holder.is_some_and(|(k, _)| k == key) {
                    self.elect(now);
                }
            }

            fn check(&mut self, key: Key, now: Monotonic) -> Result<(), Error> {
                self.due(now);
                if self.expired.contains(&key) {
                    return Err(Error::Expired);
                }
                if self.recovered.is_some() {
                    return Err(Error::Reserved);
                }
                if self.holder.is_some_and(|(k, _)| k == key) {
                    return Ok(());
                }
                Err(Error::Waiting)
            }
        }

        fn input() -> impl Strategy<Value = Input> {
            let authority = prop_oneof![Just(0), Just(100), Just(200), Just(255)];
            prop_oneof![
                (
                    0..SUBJECTS.len(),
                    authority,
                    proptest::option::of(1..30_i64)
                )
                    .prop_map(|(subject, authority, lease)| {
                        Input::Open {
                            subject,
                            authority,
                            lease,
                        }
                    }),
                any::<usize>().prop_map(Input::Close),
                any::<usize>().prop_map(Input::Check),
                any::<usize>().prop_map(Input::Write),
                Just(Input::Record),
                Just(Input::Advance),
            ]
        }

        fn start() -> impl Strategy<Value = Option<(usize, u8, i64)>> {
            let authority = prop_oneof![Just(0), Just(100), Just(200), Just(255)];
            proptest::option::of((0..SUBJECTS.len(), authority, 1..30_i64))
        }

        fn steps() -> impl Strategy<Value = Vec<(u64, Input)>> {
            proptest::collection::vec((0..20_u64, input()), 0..60)
        }

        fn check(start: Option<(usize, u8, i64)>, steps: Vec<(u64, Input)>) {
            let mut now = at(0);
            let (mut gate, mut model) = match start {
                None => (Gate::new(), Model::default()),
                Some((subject, authority, grace)) => {
                    let last = writer(SUBJECTS[subject], authority);
                    let model = Model {
                        recovered: Some((last.clone(), now + lease(grace).span())),
                        ..Model::default()
                    };
                    (Gate::recover(last, now, lease(grace)), model)
                }
            };
            let mut recorded = gate.holder().cloned();
            let mut keys: Vec<Key> = Vec::new();
            for (step, input) in steps {
                // A record is not a gate input, so it takes no time.
                if !matches!(input, Input::Record) {
                    now = Monotonic(now.0 + step);
                }
                match input {
                    Input::Open {
                        subject,
                        authority,
                        lease,
                    } => {
                        let new = writer(SUBJECTS[subject], authority);
                        let lease = lease.map(super::lease);
                        let key = gate.open(new.clone(), lease, now);
                        model.open(key, new, lease, now);
                        keys.push(key);
                    }
                    Input::Close(i) if !keys.is_empty() => {
                        let key = keys.remove(i % keys.len());
                        gate.close(key, now);
                        model.close(key, now);
                    }
                    Input::Check(i) if !keys.is_empty() => {
                        let key = keys[i % keys.len()];
                        let checked = gate.check(key, now).map(drop);
                        assert_eq!(checked, model.check(key, now));
                    }
                    Input::Write(i) if !keys.is_empty() => {
                        let key = keys[i % keys.len()];
                        let checked = model.check(key, now);
                        if checked.is_ok() {
                            model.seat(key, now);
                        }
                        assert_eq!(write(&mut gate, key, now), checked);
                    }
                    Input::Record => {
                        gate.recorded();
                        recorded = model.holder().cloned();
                    }
                    Input::Close(_)
                    | Input::Check(_)
                    | Input::Write(_)
                    | Input::Advance => {
                        gate.advance(now);
                        model.due(now);
                    }
                }
                let holder = model.holder().cloned();
                assert_eq!(gate.holder(), holder.as_ref());
                assert_eq!(gate.deadline(), model.deadline());
                assert!(gate.deadline().is_none_or(|due| now < due));
                let expected = (holder != recorded).then_some(Handoff {
                    to: holder.as_ref(),
                });
                assert_eq!(gate.handoff(), expected);
            }
        }

        /// The shrunk inputs in `proptest-regressions/gate.txt`, which its seeds no
        /// longer make since `Input` changed.
        #[test]
        fn replays_the_saved_failures() {
            let open = |subject, authority, lease| Input::Open {
                subject,
                authority,
                lease,
            };
            let mut repeated = vec![(0, open(0, 0, None)); 8];
            repeated.extend([
                (0, open(0, 255, Some(16))),
                (2, open(0, 0, None)),
                (14, open(0, 255, None)),
            ]);
            let cases = [
                (Some((0, 0, 8)), vec![(8, Input::Close(0))]),
                (Some((1, 0, 15)), vec![(0, open(1, 0, None))]),
                (None, vec![(0, open(0, 0, Some(5))), (5, Input::Advance)]),
                (None, repeated),
                (
                    Some((1, 200, 26)),
                    vec![(0, open(2, 200, None)), (0, open(1, 0, None))],
                ),
                (
                    Some((0, 0, 19)),
                    vec![(0, open(1, 0, None)), (0, Input::Write(0))],
                ),
            ];
            for (start, steps) in cases {
                check(start, steps);
            }
        }

        proptest! {
            #[test]
            fn follows_the_gate_rules(start in start(), steps in steps()) {
                check(start, steps);
            }
        }
    }
}
