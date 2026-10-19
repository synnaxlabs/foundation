use std::num::NonZeroUsize;
use std::sync::Arc;

use types::frame::key_set::KeySet;
use types::frame::{Draft, Form, Layout};

use super::{Error, Head, Time, counts};
use crate::common::rest_of_run;
use crate::hub::ends;
use crate::hub::part::{Assembly, Next, to_usize};
use crate::hub::run as items;

/// The frames of a program's write stream after the places, at the node. The node
/// receives each message and makes each draft from the pool of the home. The entries
/// of each frame index the key set: entry `n` is place `n`. It holds
/// [`Frames::bytes`] for its places, and allocates nothing after [`Frames::new`].
#[derive(Debug)]
pub struct Frames {
    places: u32,
    next: Next,
    head: Option<Head>,
    /// The counts of the frame's run of counts: for a head of [`Time::Frame`], the
    /// indexes among the ends that came.
    counts: u32,
    /// The counts that came. The run of counts is due while it is below `counts`.
    counted: u32,
    /// The counts that came before the draft, in group order. It holds one count for
    /// each place at most, so it never grows.
    early: Vec<u32>,
    assembly: Assembly<true>,
}

/// What [`Frames`] waits for next.
#[derive(Debug)]
pub enum Due<'f> {
    /// A whole message, for [`Frames::decode`]. The body of a skipped frame comes so.
    Message,
    /// Body bytes. Receive into the slice, the rest of the draft's body, and give the
    /// length to [`Frames::received`]. A message over the slice goes to
    /// [`Frames::decode`] whole, which gives its error.
    Body(&'f mut [u8]),
    /// A draft of this layout and form from the pool of the home, for
    /// [`Frames::draft`].
    Draft {
        /// The layout of the frame's ends.
        layout: Layout<'f>,
        /// The form of the head.
        form: Form,
    },
}

/// What one input gave.
#[derive(Debug)]
pub enum Step {
    /// The head of a frame. The run of its ends follows.
    Head(Head),
    /// Part of a run of ends or of a body, or a draft that the body fills.
    More,
    /// A whole frame, with the count of each present group set, for the home to
    /// write.
    Frame {
        /// The frame.
        draft: Draft,
        /// Where its stamps come from, and its path. For [`Time::Node`], the node
        /// writes the stamp over the one sample of each index before the write.
        time: Time,
    },
}

impl Frames {
    /// The bytes that a `Frames` of `places` places holds for their ends and for the
    /// counts that come before the draft.
    #[must_use]
    pub const fn bytes(places: usize) -> usize {
        Assembly::<true>::bytes(places)
            .saturating_add(places.saturating_mul(counts::LEN))
    }

    /// The frames of a write stream whose places are the entries of `set`, in order.
    ///
    /// # Panics
    ///
    /// When `set` has more than `u32::MAX` entries.
    #[must_use]
    pub fn new(set: Arc<KeySet>) -> Self {
        let places = u32::try_from(set.entries().len())
            .expect("a write stream has at most u32::MAX places");
        Self {
            places,
            next: Next::Head,
            head: None,
            counts: 0,
            counted: 0,
            early: Vec::with_capacity(to_usize(places)),
            assembly: Assembly::reserved(set),
        }
    }

    /// What the frames wait for. Ask it before each input: a call that dropped
    /// between two inputs resumes from it.
    ///
    /// # Errors
    ///
    /// When a draft is due, each [`types::frame::Error`] of [`Layout::from_ends`]. The
    /// caller then stops the stream as `MALFORMED`.
    #[inline]
    pub fn due(&mut self) -> Result<Due<'_>, types::frame::Error> {
        if self.assembly.drafting() {
            let Some(Head { form, .. }) = self.head else {
                unreachable!("invariant: a draft has a head")
            };
            return self
                .assembly
                .layout()
                .map(|layout| Due::Draft { layout, form });
        }
        if self.counted < self.counts {
            return Ok(Due::Message);
        }
        Ok(self
            .assembly
            .rest(self.next.body())
            .map_or(Due::Message, Due::Body))
    }

    /// Takes the next message, whole. A body message is copied into the draft, or
    /// dropped for a skipped frame. When the program finishes its stream, call
    /// [`Frames::finish`].
    ///
    /// # Errors
    ///
    /// For a head, in order: [`Error::Empty`], [`Error::Kind`] for a kind other than
    /// the head, [`Error::Length`], [`Error::Path`] for a time that no head has,
    /// [`Error::Form`], [`Error::Series`], [`Error::Stamp`], and [`Error::Places`] for
    /// more series than places. For a message of the run of ends, the errors of its
    /// ends, then [`Error::Run`], then, for the counts after its last end, the errors
    /// of a message of the run of counts. For a message of the run of counts, the
    /// errors of its bytes, then [`Error::Run`] and [`Error::Samples`]. For a body
    /// message, [`Error::Empty`] and [`Error::Body`]. The frames stay as they were. The
    /// caller then stops the stream as `MALFORMED`.
    ///
    /// # Panics
    ///
    /// When [`Frames::due`] gives [`Due::Draft`].
    #[inline]
    pub fn decode(&mut self, message: &[u8]) -> Result<Step, Error> {
        self.assembly.check_undrafted("the next message");
        match self.next {
            Next::Ends { remain } => self.ends(remain, message),
            _ if self.counted < self.counts => self.counts(message),
            Next::Head => self.head(message),
            Next::Body(body) => {
                let last = self.next.take(message.len())?;
                self.assembly.body(body.at(), message);
                Ok(self.received_body(last))
            }
        }
    }

    /// Takes a body message of `len` bytes that the caller wrote at the start of the
    /// slice of [`Due::Body`].
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `len` is 0. The caller then stops the stream as
    /// `MALFORMED`.
    ///
    /// # Panics
    ///
    /// When [`Frames::due`] does not give [`Due::Body`], or `len` is over its slice.
    #[inline]
    pub fn received(&mut self, len: usize) -> Result<Step, Error> {
        self.assembly.check_body();
        let last = self.next.received(len)?;
        Ok(self.received_body(last))
    }

    /// Takes the draft, which is of the layout and form of [`Due::Draft`]. Gives the
    /// frame when no count and no body is still to come, else [`Step::More`].
    ///
    /// # Panics
    ///
    /// When [`Frames::due`] does not give [`Due::Draft`].
    #[inline]
    pub fn draft(&mut self, draft: Draft) -> Step {
        self.assembly.draft(draft);
        self.drafted()
    }

    /// Takes no draft for the frame whose draft is due, when the pool of the home has
    /// no block for it. The frame's body, when it has one, then comes as
    /// [`Due::Message`], and [`Frames::decode`] checks each message, drops it, and
    /// gives [`Step::More`].
    ///
    /// # Panics
    ///
    /// When [`Frames::due`] does not give [`Due::Draft`].
    #[inline]
    pub fn skip(&mut self) {
        self.assembly.skip();
        self.drafted();
    }

    /// Checks the end of the stream, when the program finished it.
    ///
    /// # Errors
    ///
    /// [`Error::Cut`] inside a frame. The caller then stops the stream as `MALFORMED`.
    ///
    /// # Panics
    ///
    /// When [`Frames::due`] gives [`Due::Draft`].
    pub fn finish(&self) -> Result<(), Error> {
        self.assembly.check_undrafted("the end of the stream");
        let (remain, body) = match self.next {
            Next::Head if self.counted == self.counts => return Ok(()),
            Next::Head => (0, None),
            Next::Ends { remain } => (remain, None),
            Next::Body(body) => (0, Some(body)),
        };
        let head = self.head.expect("invariant: a frame has a head");
        let ends = head
            .series
            .checked_sub(remain)
            .expect("invariant: the ends that remain are of the head's series");
        let fit = "invariant: the bytes of a frame fit a usize";
        let sum = |a: usize, b: usize| a.checked_add(b).expect(fit);
        let product = |a: usize, b: usize| a.checked_mul(b).expect(fit);
        let run = sum(Head::LEN, product(to_usize(ends), ends::LEN));
        if remain > 0 {
            return Err(Error::Cut {
                received: run,
                len: None,
            });
        }
        let (at, len) =
            body.map_or((0, 0), |body| (body.at(), sum(body.at(), body.remain())));
        let counts = |n| product(to_usize(n), counts::LEN);
        let counted = sum(run, counts(self.counted));
        let whole = sum(run, counts(self.counts));
        Err(Error::Cut {
            received: sum(counted, at),
            len: Some(
                NonZeroUsize::new(sum(whole, len))
                    .expect("invariant: a frame holds its head"),
            ),
        })
    }

    fn head(&mut self, message: &[u8]) -> Result<Step, Error> {
        let head = Head::decode(message)?;
        if head.series > self.places {
            return Err(Error::Places {
                series: head.series,
                places: self.places,
            });
        }
        self.head = Some(head);
        self.counts = 0;
        self.counted = 0;
        self.assembly.head();
        self.next = Next::Ends {
            remain: head.series,
        };
        Ok(Step::Head(head))
    }

    /// Takes a message of the run of ends, which `remain` ends of the head's series
    /// did not reach. For a head of [`Time::Frame`], each index among its ends adds a
    /// count to the run, and the bytes after its last end are the first counts.
    fn ends(&mut self, remain: u32, message: &[u8]) -> Result<Step, Error> {
        let time = self.head.expect("invariant: ends follow a head").time;
        let (message, early) = match time {
            Time::Frame(_) => message.split_at(
                to_usize(remain)
                    .saturating_mul(ends::LEN)
                    .min(message.len()),
            ),
            Time::Node => (message, &[][..]),
        };
        let mut next = self.next;
        let (ends, last) = next.ends(message)?;
        let counts = match time {
            Time::Frame(_) => self.counts.strict_add(self.indexes(ends.clone())),
            Time::Node => 0,
        };
        let (early, uncounted) = if early.is_empty() {
            (&[][..], counts)
        } else {
            Self::checked(early, counts)?
        };
        self.next = next;
        self.counts = counts;
        self.assembly.ends(ends, last);
        self.early
            .extend(early.iter().map(|&bytes| u32::from_le_bytes(bytes)));
        self.counted = counts.strict_sub(uncounted);
        Ok(Step::More)
    }

    /// The ends among `ends` whose place is the index of its group. A place that the
    /// set does not hold is none: the layout refuses it.
    fn indexes(&self, ends: ends::Iter<'_>) -> u32 {
        let set = self.assembly.set();
        let indexes = ends
            .filter(|&(place, _)| {
                let place = to_usize(place);
                set.entries().get(place).is_some_and(|entry| {
                    set.groups().get(to_usize(entry.group)) == Some(&place)
                })
            })
            .count();
        u32::try_from(indexes)
            .expect("invariant: a message holds at most u32::MAX ends")
    }

    /// Starts the run of counts of a draft or a skip with the counts that came before
    /// it. A head of [`Time::Node`] has none, and each of its ranges holds 1.
    fn drafted(&mut self) -> Step {
        match self.head.expect("invariant: a draft has a head").time {
            Time::Frame(_) => self.assembly.count(0, self.early.drain(..)),
            Time::Node => self.assembly.count(0, std::iter::repeat(1)),
        }
        self.counted()
    }

    /// Takes a message of the run of counts.
    fn counts(&mut self, message: &[u8]) -> Result<Step, Error> {
        let (counts, _) = Self::checked(message, self.uncounted())?;
        Ok(self.fill(counts))
    }

    /// The counts of `message`, of a run in which `remain` counts did not come, and
    /// the counts that do not come after them.
    fn checked(
        message: &[u8],
        remain: u32,
    ) -> Result<(&[[u8; counts::LEN]], u32), Error> {
        let counts = items::<{ counts::LEN }>(message)?;
        let rest = rest_of_run(remain, counts.len())?;
        if counts.contains(&[0; counts::LEN]) {
            return Err(Error::Samples);
        }
        Ok((counts, rest))
    }

    /// The counts of the run that did not come.
    fn uncounted(&self) -> u32 {
        self.counts
            .checked_sub(self.counted)
            .expect("invariant: no more counts came than are due")
    }

    /// Sets the next counts that are due from `counts`, which do not pass the run.
    fn fill(&mut self, counts: &[[u8; counts::LEN]]) -> Step {
        self.assembly.count(
            to_usize(self.counted),
            counts.iter().map(|&bytes| u32::from_le_bytes(bytes)),
        );
        let came = u32::try_from(counts.len())
            .expect("invariant: no more counts came than are due");
        self.counted = self
            .counted
            .checked_add(came)
            .expect("invariant: no more counts came than are due");
        self.counted()
    }

    /// The frame when its counts came and it has no body, else [`Step::More`].
    fn counted(&mut self) -> Step {
        match self.next.body() {
            None if self.counted == self.counts => self.frame(),
            _ => Step::More,
        }
    }

    /// The step of the body message that arrived, the frame when it is the `last`.
    fn received_body(&mut self, last: bool) -> Step {
        if last { self.frame() } else { Step::More }
    }

    /// The frame of the head and the draft that arrived, or [`Step::More`] when the
    /// frame was skipped.
    #[inline]
    fn frame(&mut self) -> Step {
        let head = self.head.take().expect("invariant: a frame has a head");
        let Some(draft) = self.assembly.take() else {
            return Step::More;
        };
        Step::Frame {
            draft,
            time: head.time,
        }
    }
}

#[cfg(test)]
mod tests;
