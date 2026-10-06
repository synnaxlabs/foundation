//! One end of an RTU line: its port, and when the line was last busy.

use env::clock::Clock;
use env::serial::{Error, Port, Settings};
use types::time::{Monotonic, Rate, Span};

use crate::wait::within;

/// The quiet above 19,200 baud, which the specification fixes.
const FAST: Span = Span::from_nanos(1_750_000);

#[derive(Debug)]
pub(crate) struct Line {
    port: Port,
    clock: Clock,
    rate: Rate,
    /// The span of quiet that ends a frame.
    quiet: Span,
    /// When the last byte, sent or read, left the line.
    busy: Monotonic,
}

impl Line {
    pub(crate) fn new(port: Port, clock: Clock, settings: Settings) -> Self {
        let rate = settings.rate();
        let quiet = if settings.baud.get() > 19_200 {
            FAST
        } else {
            Span::from_nanos(rate.span(7).nanos().div_euclid(2))
        };
        let busy = clock.now();
        Self {
            port,
            clock,
            rate,
            quiet,
            busy,
        }
    }

    pub(crate) fn now(&self) -> Monotonic {
        self.clock.now()
    }

    /// When the line has been quiet long enough to end a frame.
    pub(crate) fn rested(&self) -> Monotonic {
        self.busy
            .checked_add(self.quiet)
            .expect("invariant: a clock reading plus 3.5 characters fits a u64")
    }

    /// Reads the bytes that arrived into the end of `bytes`, and gives `false` when
    /// none came before `deadline`.
    pub(crate) async fn read(
        &mut self,
        bytes: &mut Vec<u8>,
        deadline: Option<Monotonic>,
    ) -> Result<bool, Error> {
        let mut buffer = [0; 256];
        let read = within(&self.clock, deadline, |cx| {
            self.port.poll_read(cx, &mut buffer)
        });
        let Some(n) = read.await.transpose()? else {
            return Ok(false);
        };
        self.busy = self.clock.now();
        bytes.extend(buffer.iter().take(n));
        Ok(true)
    }

    /// Sends all of `bytes`, and gives `false` when the send queue did not take them
    /// before `deadline`.
    pub(crate) async fn write(
        &mut self,
        bytes: &[u8],
        deadline: Option<Monotonic>,
    ) -> Result<bool, Error> {
        let mut rest = bytes;
        while !rest.is_empty() {
            let write =
                within(&self.clock, deadline, |cx| self.port.poll_write(cx, rest));
            let Some(n) = write.await.transpose()? else {
                return Ok(false);
            };
            let characters = u64::try_from(n).expect("invariant: a write fits a u64");
            let span = self.rate.span(characters);
            self.busy = (self.busy.max(self.clock.now()).checked_add(span))
                .expect("invariant: a clock reading plus 256 characters fits a u64");
            rest = rest.get(n..).unwrap_or_default();
        }
        Ok(true)
    }

    /// Drops the bytes that arrive until `after`, and then until the line was quiet
    /// long enough to end a frame. Call it before each frame sent. Gives `false`
    /// when the line cannot be quiet before `deadline`.
    pub(crate) async fn rest(
        &mut self,
        after: Option<Monotonic>,
        deadline: Option<Monotonic>,
    ) -> Result<bool, Error> {
        let mut dropped = Vec::new();
        loop {
            let rested = self.rested();
            let until = after.map_or(rested, |after| after.max(rested));
            if deadline.is_some_and(|deadline| until >= deadline) {
                return Ok(false);
            }
            if !self.read(&mut dropped, Some(until)).await? {
                return Ok(true);
            }
            dropped.clear();
        }
    }
}
