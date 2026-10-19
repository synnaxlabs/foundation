//! The parts of a frame after its head, in a read and in a write: the run of its ends,
//! then its body when the last end is not 0.

use std::sync::Arc;

use types::frame::key_set::KeySet;
use types::frame::{Draft, Layout};

use super::{Error, ends};
use crate::common::{body, rest_of_run};

/// The part of a frame that a stream expects next.
#[derive(Clone, Copy, Debug)]
pub(super) enum Next {
    /// The head of the next frame.
    Head,
    /// A message of the run of ends, with the ends that remain in the run.
    Ends { remain: u32 },
    /// A message of the body.
    Body(body::Count),
}

impl Next {
    /// Decodes `message` as a message of the run. Gives the ends of the message, and
    /// whether the run ends with them.
    ///
    /// # Errors
    ///
    /// The errors of [`ends::decode`], then [`Error::Run`] for more ends than remain.
    /// The part stays as it was.
    ///
    /// # Panics
    ///
    /// When the next message is not of the run.
    #[inline]
    pub(super) fn ends<'m>(
        &mut self,
        message: &'m [u8],
    ) -> Result<(ends::Iter<'m>, bool), Error> {
        let Self::Ends { remain } = *self else {
            panic!("a run of ends is due, not {self:?}")
        };
        let ends = ends::decode(message)?;
        let remain = rest_of_run(remain, ends.len())?;
        *self = match (remain, ends.last_end()) {
            (0, Some(0)) => Self::Head,
            (0, Some(end)) => Self::Body(body::Count::new(to_usize(end))),
            _ => Self::Ends { remain },
        };
        Ok((ends, remain == 0))
    }

    /// Takes a body message of `len` bytes that the caller has in place, as
    /// [`Next::take`] takes it. Gives whether it is the last of the body.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `len` is 0.
    ///
    /// # Panics
    ///
    /// When the next message is not body bytes, or `len` is over the rest of the
    /// body.
    #[inline]
    pub(super) fn received(&mut self, len: usize) -> Result<bool, Error> {
        if let Self::Body(body) = *self {
            let remain = body.remain();
            assert!(
                len <= remain,
                "a body message of {len} bytes is over the {remain} bytes left of its \
                 body"
            );
        }
        self.take(len)
    }

    /// Takes a body message of `len` bytes. Gives whether it is the last of the body.
    ///
    /// # Errors
    ///
    /// [`Error::Empty`] when `len` is 0, and [`Error::Body`] when it is over the rest
    /// of the body. The part stays as it was.
    ///
    /// # Panics
    ///
    /// When the next message is not body bytes.
    #[inline]
    pub(super) fn take(&mut self, len: usize) -> Result<bool, Error> {
        let Self::Body(mut body) = *self else {
            panic!("a body is due, not {self:?}")
        };
        let last = body.take(len)?;
        *self = if last { Self::Head } else { Self::Body(body) };
        Ok(last)
    }

    /// Where in the body the next message starts, when the next message is body
    /// bytes.
    #[inline]
    pub(super) fn body(self) -> Option<usize> {
        match self {
            Self::Body(body) => Some(body.at()),
            Self::Head | Self::Ends { .. } => None,
        }
    }

    /// The bytes of the body that did not come, when the stream ends inside a body.
    pub(super) fn unfinished(self) -> Option<usize> {
        match self {
            Self::Body(body) => Some(body.remain()),
            Self::Head | Self::Ends { .. } => None,
        }
    }
}

/// The ends and the draft of the frame that arrives, from its run of ends to the last
/// byte of its body. `SKIP` is whether it can skip a frame: with `false`, as the read
/// has it, no code of the skipped stage is built.
#[derive(Debug)]
pub(super) struct Assembly<const SKIP: bool> {
    set: Arc<KeySet>,
    /// The entry and end of each series.
    ends: Vec<(usize, usize)>,
    stage: Stage,
}

/// Where the frame that arrives is, after its head.
#[derive(Debug)]
enum Stage {
    /// In the run of ends, or between frames.
    Run,
    /// The ends are whole and the draft is not given.
    Drafting,
    /// The ends are whole and their layout is not valid, so no draft comes.
    Refused,
    /// The body fills the draft.
    Body(Draft),
    /// The body of a skipped frame, which is dropped.
    Skipped,
}

impl<const SKIP: bool> Assembly<SKIP> {
    /// The ends grow to those of the widest frame.
    pub(super) fn new(set: Arc<KeySet>) -> Self {
        Self {
            set,
            ends: Vec::new(),
            stage: Stage::Run,
        }
    }

    /// The bytes that [`Assembly::reserved`] holds for the ends of `places` places.
    pub(super) const fn bytes(places: usize) -> usize {
        places.saturating_mul(size_of::<(usize, usize)>())
    }

    /// Makes room for one end per place, so no frame allocates.
    pub(super) fn reserved(set: Arc<KeySet>) -> Self {
        Self {
            ends: Vec::with_capacity(set.entries().len()),
            ..Self::new(set)
        }
    }

    pub(super) fn set(&self) -> &Arc<KeySet> {
        &self.set
    }

    /// Whether a draft is due, so that [`Assembly::layout`] gives its layout.
    #[inline]
    pub(super) fn drafting(&self) -> bool {
        matches!(self.stage, Stage::Drafting | Stage::Refused)
    }

    /// The layout of the draft that is due. An error refuses each later draft and skip.
    ///
    /// # Panics
    ///
    /// When no draft is due.
    #[inline]
    pub(super) fn layout(&mut self) -> Result<Layout<'_>, types::frame::Error> {
        assert!(self.drafting(), "no draft is due");
        Layout::from_ends(&self.set, &self.ends).inspect_err(|_| {
            self.stage = Stage::Refused;
        })
    }

    /// The rest of the draft's body, when the next body message starts at `body`.
    /// `None` when a whole message is due.
    #[inline]
    pub(super) fn rest(&mut self, body: Option<usize>) -> Option<&mut [u8]> {
        let Stage::Body(draft) = &mut self.stage else {
            return None;
        };
        let Some(start) = body else {
            unreachable!("invariant: a draft is taken at its last byte")
        };
        Some(rest(draft, start))
    }

    /// Starts the frame of a head.
    pub(super) fn head(&mut self) {
        self.ends.clear();
        self.stage = Stage::Run;
    }

    /// Takes the ends of a run message. The draft is due after the `last`.
    #[inline]
    pub(super) fn ends(&mut self, ends: ends::Iter<'_>, last: bool) {
        self.ends
            .extend(ends.map(|(place, end)| (to_usize(place), to_usize(end))));
        if last {
            self.stage = Stage::Drafting;
        }
    }

    /// The group of each index among the ends, in order, which is group order: one for
    /// each range of the draft. Call it once the layout is valid.
    pub(super) fn groups(&self) -> impl Iterator<Item = u32> {
        groups(&self.set, &self.ends)
    }

    /// Sets the counts of the ranges of the draft in order, from range `first` on. A
    /// skipped frame drops them.
    ///
    /// # Panics
    ///
    /// When neither a draft nor a skip came.
    pub(super) fn count(
        &mut self,
        first: usize,
        counts: impl IntoIterator<Item = u32>,
    ) {
        let draft = match &mut self.stage {
            Stage::Body(draft) => draft,
            Stage::Skipped if SKIP => return,
            stage => {
                unreachable!("invariant: counts come after a draft, not {stage:?}")
            }
        };
        for (group, count) in groups(&self.set, &self.ends).skip(first).zip(counts) {
            draft.set_count(group, count);
        }
    }

    /// Copies body bytes that start at `start` into the draft. A skipped frame drops
    /// them.
    #[inline]
    pub(super) fn body(&mut self, start: usize, bytes: &[u8]) {
        match &mut self.stage {
            Stage::Body(draft) => {
                prefix(rest(draft, start), bytes.len()).copy_from_slice(bytes);
            }
            Stage::Skipped if SKIP => {}
            stage => {
                unreachable!("invariant: body bytes come after a draft, not {stage:?}")
            }
        }
    }

    /// Panics with `what` when a draft is due.
    #[inline]
    pub(super) fn check_undrafted(&self, what: &str) {
        if matches!(self.stage, Stage::Drafting | Stage::Refused) {
            due_draft(what);
        }
    }

    /// Panics unless body bytes are due into a draft.
    #[inline]
    pub(super) fn check_body(&self) {
        assert!(matches!(self.stage, Stage::Body(_)), "a body is due");
    }

    /// Takes the draft that is due.
    ///
    /// # Panics
    ///
    /// When no draft is due.
    #[inline]
    pub(super) fn draft(&mut self, draft: Draft) {
        self.check_drafting();
        self.stage = Stage::Body(draft);
    }

    /// Takes no draft for the frame whose draft is due, and drops its body.
    ///
    /// # Panics
    ///
    /// When no draft is due.
    #[inline]
    pub(super) fn skip(&mut self) {
        const { assert!(SKIP, "an assembly that skips no frame takes no skip") };
        self.check_drafting();
        self.stage = Stage::Skipped;
    }

    /// The draft of the frame that arrived, `None` for a skipped frame.
    ///
    /// # Panics
    ///
    /// When the frame has neither a draft nor a skip.
    #[inline]
    pub(super) fn take(&mut self) -> Option<Draft> {
        match std::mem::replace(&mut self.stage, Stage::Run) {
            Stage::Body(draft) => Some(draft),
            Stage::Skipped if SKIP => None,
            stage => {
                unreachable!("invariant: a frame ends after a draft, not {stage:?}")
            }
        }
    }

    fn check_drafting(&self) {
        assert!(matches!(self.stage, Stage::Drafting), "no draft is due");
    }
}

#[cold]
#[inline(never)]
fn due_draft(what: &str) -> ! {
    panic!("a draft is due before {what}")
}

/// The group of each index among `ends`, in order. Each entry of `ends` is in `set`.
fn groups<'a>(
    set: &'a KeySet,
    ends: &'a [(usize, usize)],
) -> impl Iterator<Item = u32> + 'a {
    let entries = set.entries();
    ends.iter().filter_map(move |&(entry, _)| {
        let place = entries
            .get(entry)
            .expect("invariant: each end is of the set");
        (set.index(entry) == entry).then_some(place.group)
    })
}

/// The body of `draft` from `start` on.
fn rest(draft: &mut Draft, start: usize) -> &mut [u8] {
    let body = draft.body_mut();
    let len = body.len();
    body.get_mut(start..).unwrap_or_else(|| {
        panic!(
            "invariant: a body that starts at {start} is inside its {len} byte draft"
        )
    })
}

/// The first `len` bytes of `rest`.
fn prefix(rest: &mut [u8], len: usize) -> &mut [u8] {
    let room = rest.len();
    rest.get_mut(..len).unwrap_or_else(|| {
        panic!(
            "a body message of {len} bytes is over the {room} bytes left of its body"
        )
    })
}

/// `n` as a `usize`.
pub(super) fn to_usize(n: u32) -> usize {
    usize::try_from(n).expect("invariant: a usize holds a u32")
}
