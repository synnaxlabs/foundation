use std::fmt;
use std::num::NonZeroU8;

use env::clock::Clock;
use env::serial::{self, Serial};
use types::time::{Monotonic, Span};

use super::line::Line;
use crate::Error;
use crate::pdu::{Reply, Request};

/// A Modbus RTU client on one serial port: one request at a time, each with its
/// reply.
#[derive(Debug)]
pub struct Client {
    line: Line,
    timeout: Span,
    bytes: Vec<u8>,
    /// The deadline of an exchange that was dropped before its end. Its reply can
    /// still come until then.
    stale: Option<Monotonic>,
}

impl Client {
    /// Opens the port at `config.path`. Each exchange gives up after `timeout`. A
    /// timeout below zero acts as zero.
    ///
    /// # Errors
    ///
    /// The error of [`Serial::open`].
    pub async fn open(
        serial: &Serial,
        config: &serial::Config,
        clock: Clock,
        timeout: Span,
    ) -> Result<Self, serial::Error> {
        let port = serial.open(config).await?;
        Ok(Self {
            line: Line::new(port, clock, config.settings),
            timeout: timeout.max(Span::ZERO),
            bytes: Vec::new(),
            stale: None,
        })
    }

    /// Sends `request` to `unit` and reads its reply. It first waits until the line
    /// was quiet for 3.5 characters (1.75 ms above 19,200 baud), as the
    /// specification asks between frames, and drops the bytes that arrive in that
    /// time. The timeout covers the quiet, the request, and the reply.
    ///
    /// It is safe to drop: after a dropped exchange, the next one drops bytes until
    /// the dropped one's deadline, so a late reply never reads as its own. Its own
    /// timeout starts at that deadline.
    ///
    /// # Errors
    ///
    /// - [`Failure::Timeout`] when no whole reply came within the timeout.
    /// - [`Failure::Frame`] with the codec error of a reply that is not valid: a bad
    ///   CRC, or a reply that does not match the request.
    /// - [`Failure::Request`] with the error of [`encode`](super::encode) for a
    ///   request that is not valid. Nothing is sent then.
    /// - [`Failure::Unit`] for a reply from another unit.
    /// - [`Failure::Serial`] when the port failed. The client is no use then.
    #[expect(
        clippy::missing_panics_doc,
        reason = "the second decode gives the whole frame the first found"
    )]
    pub async fn exchange(
        &mut self,
        unit: NonZeroU8,
        request: &Request,
    ) -> Result<Reply<'_>, Failure> {
        self.bytes.clear();
        super::encode(unit.get(), request, &mut self.bytes)
            .map_err(Failure::Request)?;
        let (stale, now) = (self.stale, self.line.now());
        // A timeout past the end of the clock never ends.
        let deadline = stale
            .map_or(now, |stale| stale.max(now))
            .checked_add(self.timeout);
        self.stale = deadline;
        let read = self.ask(request, stale, deadline).await;
        self.stale = None;
        read?;
        let frame = super::decode_reply(request, &self.bytes)?
            .expect("invariant: the loop ends at a whole frame");
        if frame.unit != unit.get() {
            return Err(Failure::Unit {
                want: unit.get(),
                got: frame.unit,
            });
        }
        Ok(request.decode_reply(frame.pdu)?)
    }

    /// Sends the frame in `bytes` after the quiet, and reads into `bytes` until it
    /// holds one whole reply to `request`.
    async fn ask(
        &mut self,
        request: &Request,
        stale: Option<Monotonic>,
        deadline: Option<Monotonic>,
    ) -> Result<(), Failure> {
        if !self.line.rest(stale, deadline).await?
            || !self.line.write(&self.bytes, deadline).await?
        {
            return Err(Failure::Timeout);
        }
        self.bytes.clear();
        while super::decode_reply(request, &self.bytes)?.is_none() {
            if !self.line.read(&mut self.bytes, deadline).await? {
                return Err(Failure::Timeout);
            }
        }
        Ok(())
    }
}

/// Why an exchange gave no reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The port failed.
    Serial(serial::Error),
    /// No whole reply came within the timeout.
    Timeout,
    /// A reply that is not valid.
    Frame(Error),
    /// A request that is not valid. It fails the same way each time.
    Request(Error),
    /// A reply from another unit than the request's.
    Unit {
        /// The request's unit.
        want: u8,
        /// The reply's unit.
        got: u8,
    },
}

impl From<serial::Error> for Failure {
    fn from(error: serial::Error) -> Self {
        Self::Serial(error)
    }
}

impl From<Error> for Failure {
    fn from(error: Error) -> Self {
        Self::Frame(error)
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Serial(error) => error.fmt(f),
            Self::Timeout => write!(f, "no whole reply came before the timeout"),
            Self::Frame(error) | Self::Request(error) => error.fmt(f),
            Self::Unit { want, got } => {
                write!(f, "a reply from unit {got} to a request to unit {want}")
            }
        }
    }
}

impl std::error::Error for Failure {}
