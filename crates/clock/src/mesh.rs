use estimate::combine::{self, combine};
use estimate::discipline::{Cause, Discipline};
use estimate::{Filter, Measurement, Slew};
use types::hash::Map;
use types::time::{Interval, Monotonic};

use crate::{DRIFT, cell, source};

/// The mesh clock of one node. It lives on one shard, and [`Reader`]s read it from
/// any.
#[derive(Debug)]
pub struct Clock {
    monotonic: env::clock::Clock,
    sources: Map<source::Key, Filter>,
    next: u64,
    discipline: Discipline,
    cell: cell::Writer,
}

impl Clock {
    /// Makes a clock with no sources, and a reader for it. Mesh time runs on
    /// `monotonic`, the node's monotonic clock. Readers have no time until a majority
    /// of the sources first agree.
    #[must_use]
    pub fn new(monotonic: env::clock::Clock) -> (Self, Reader) {
        let discipline = Discipline::Unsynced(combine::Error::NoSources);
        let (cell, cell_reader) = cell::new(discipline);
        let reader = Reader {
            monotonic: monotonic.clone(),
            cell: cell_reader,
        };
        let clock = Self {
            monotonic,
            sources: Map::default(),
            next: 0,
            discipline,
            cell,
        };
        (clock, reader)
    }

    /// Adds a source with no measurements, then moves mesh time toward the sources,
    /// and returns its key. The source counts against a majority until it pushes.
    pub fn add(&mut self) -> source::Key {
        let key = source::Key(self.next);
        self.next += 1;
        self.sources.insert(key, Filter::default());
        self.steer();
        key
    }

    /// Removes a source and its measurements, then moves mesh time toward the sources
    /// left.
    ///
    /// # Panics
    ///
    /// When `source` was removed already.
    pub fn remove(&mut self, source: source::Key) {
        let removed = self.sources.remove(&source);
        assert!(removed.is_some(), "{source:?} was removed");
        self.steer();
    }

    /// Records a measurement of the monotonic clock from `source`, then moves mesh
    /// time toward the estimate of all sources.
    ///
    /// # Panics
    ///
    /// When `source` was removed.
    pub fn push(&mut self, source: source::Key, measurement: Measurement) {
        let Some(filter) = self.sources.get_mut(&source) else {
            panic!("{source:?} was removed");
        };
        filter.push(measurement);
        self.steer();
    }

    /// Feeds the clock from the node's time sources, today the OS clock `wall`. Each
    /// source measures when its adapter decides: the OS clock at once, then a second
    /// after the last. It never returns; drop the future to stop it.
    ///
    /// # Panics
    ///
    /// When the clock has a source already, because `run` owns every source. On a
    /// thread that `env` did not start. When the OS bound is negative, or the
    /// monotonic clock goes back.
    pub async fn run(mut self, wall: env::wall::Wall) -> ! {
        assert!(self.sources.is_empty(), "a source was added before run");
        let mut wall = source::Wall::new(wall, self.monotonic.clone());
        let source = self.add();
        loop {
            let measurement = wall.next().await;
            self.push(source, measurement);
        }
    }

    fn steer(&mut self) {
        let estimate = combine(self.monotonic.now(), DRIFT, self.sources.values());
        // A write that changes nothing makes the reads that overlap it run again.
        let Some(change) = self.discipline.next(estimate, DRIFT) else {
            return;
        };
        // A slew keeps mesh time from going back only against reads before its `now`,
        // so the clock reads inside the update.
        self.cell.update(|| {
            self.discipline = change.at(self.monotonic.now(), DRIFT);
            self.discipline
        });
    }
}

/// What a clock follows after its last [`Clock::add`], [`Clock::remove`], or
/// [`Clock::push`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// No majority of the sources has agreed yet, so readers have no time. A source
    /// with no measurement counts against a majority. Holds why.
    Unsynced(combine::Error),
    /// A majority of the sources agrees. Holds mesh time now, as an offset from the
    /// monotonic clock with its error. The error can be unknown
    /// ([`Measurement::unknown`]).
    Synced(Measurement),
    /// Mesh time keeps its slew, and the error of the slew's target grows by drift.
    /// Holds mesh time now and why.
    Holdover(Measurement, Cause),
}

/// Reads mesh time from any thread with no lock. Clones read the same clock.
#[derive(Clone, Debug)]
pub struct Reader {
    monotonic: env::clock::Clock,
    cell: cell::Reader,
}

impl Reader {
    /// Mesh time now. The true time is inside the interval. Its midpoint is the
    /// clock's best guess, and it never goes back: a call that starts after another
    /// returns gives a midpoint no earlier, while both edges fit a stamp (from 1777 to
    /// 2162 with an unknown error). `None` until a majority of the clock's sources
    /// first agree.
    #[must_use]
    pub fn now(&self) -> Option<Interval> {
        self.cell.read(|discipline| {
            let slew = discipline.slew()?;
            Some(slew.at(self.monotonic.now(), DRIFT).interval())
        })
    }

    /// What the clock follows now, with mesh time at the call. It holds the result of
    /// the last [`Clock::add`], [`Clock::remove`], or [`Clock::push`] to return. That
    /// mesh time is the one [`Reader::now`] gives, so it never goes back.
    /// [`Status::Unsynced`] until a majority of the sources first agree.
    #[must_use]
    pub fn status(&self) -> Status {
        self.cell.read(|discipline| {
            let at = |slew: Slew| slew.at(self.monotonic.now(), DRIFT);
            match discipline {
                Discipline::Unsynced(error) => Status::Unsynced(error),
                Discipline::Synced(slew) => Status::Synced(at(slew)),
                Discipline::Holdover(slew, cause) => Status::Holdover(at(slew), cause),
            }
        })
    }

    /// The clock's first estimate at `reading`, a reading of the node's monotonic
    /// clock: its offset, with its error grown by drift to `reading` (200 ppm, 0.72 s
    /// in one hour). Later estimates never change it. It stamps a reading taken
    /// before a call to [`Reader::now`] that gave `None`: its midpoint is then never
    /// later than the midpoint of an interval from [`Reader::now`], while the edges
    /// of both fit a stamp (from 1777 to 2162 with an unknown error). `None` until a
    /// majority of the clock's sources first agree. A call that starts after
    /// [`Reader::now`] gave an interval gives one.
    #[must_use]
    pub fn first(&self, reading: Monotonic) -> Option<Interval> {
        let slew = self.cell.first()?;
        Some(slew.at(reading, DRIFT).interval())
    }
}
