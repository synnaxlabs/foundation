use std::fmt;
use std::io::IoSlice;

use env::clock::Clock;
use env::net::{self, Net, Tcp};
use types::time::{Monotonic, Span};

use super::Header;
use crate::Error;
use crate::pdu::{Reply, Request};
use crate::wait::within;

/// A Modbus TCP client on one stream: one request at a time, each with its reply.
#[derive(Debug)]
pub struct Client {
    stream: Tcp,
    clock: Clock,
    timeout: Span,
    /// The transaction number of the last request.
    transaction: u16,
    /// Request bytes not yet sent.
    unsent: Vec<u8>,
    /// The bytes at the front of `unsent` that must go out: the rest of requests
    /// that left in part. The bytes after them belong to a request none of which
    /// left, and the next exchange drops them.
    owed: usize,
    /// Bytes read and not yet used.
    received: Vec<u8>,
}

impl Client {
    /// Connects to `config.remote`. Each exchange gives up after `timeout`. A
    /// timeout below zero acts as zero.
    ///
    /// # Errors
    ///
    /// The error of [`Net::connect`].
    pub async fn connect(
        net: &Net,
        config: &net::tcp::Config,
        clock: Clock,
        timeout: Span,
    ) -> Result<Self, net::Error> {
        let stream = net.connect(config).await?;
        Ok(Self {
            stream,
            clock,
            timeout: timeout.max(Span::ZERO),
            transaction: 0,
            unsent: Vec::new(),
            owed: 0,
            received: Vec::new(),
        })
    }

    /// Sends `request` to `unit` and reads its reply. Each request gets the next
    /// transaction number, and a reply with another number is dropped.
    ///
    /// It is safe to drop. A request none of which left is dropped with it. The next
    /// exchange first sends the rest of a request that left in part, so the stream
    /// stays in step, and that request's reply is dropped by its number.
    ///
    /// # Errors
    ///
    /// - [`Failure::Timeout`] when no whole reply came within the timeout. The
    ///   request can still take effect, unless none of it left.
    /// - [`Failure::Frame`] with the codec error of a reply that does not match the
    ///   request.
    /// - [`Failure::Request`] with the error of [`encode`](super::encode) for a
    ///   request that is not valid. Nothing is sent then.
    /// - [`Failure::Unit`] for a reply from another unit.
    /// - [`Failure::Net`], [`Failure::Closed`], or [`Failure::Stream`] when the
    ///   stream failed, ended, or is out of step. The client is no use then.
    #[expect(
        clippy::missing_panics_doc,
        reason = "receive gives the length of a whole frame it found"
    )]
    pub async fn exchange(
        &mut self,
        unit: u8,
        request: &Request,
    ) -> Result<Reply<'_>, Failure> {
        self.unsent.truncate(self.owed);
        let header = Header {
            transaction: self.transaction.wrapping_add(1),
            unit,
        };
        super::encode(header, request, &mut self.unsent).map_err(Failure::Request)?;
        self.transaction = header.transaction;
        // A timeout past the end of the clock never ends.
        let deadline = self.clock.now().checked_add(self.timeout);
        self.send(deadline).await?;
        let (got, len) = self.receive(header.transaction, deadline).await?;
        if got != unit {
            return Err(Failure::Unit { want: unit, got });
        }
        let pdu = self.received.get(super::HEADER..len);
        let pdu = pdu.expect("invariant: receive found a whole frame");
        request.decode_reply(pdu).map_err(Failure::Frame)
    }

    /// Sends the bytes in `unsent`.
    async fn send(&mut self, deadline: Option<Monotonic>) -> Result<(), Failure> {
        while !self.unsent.is_empty() {
            let parts = [IoSlice::new(&self.unsent)];
            let write = within(&self.clock, deadline, |cx| {
                self.stream.poll_write(cx, &parts)
            });
            let n = write.await.ok_or(Failure::Timeout)??;
            self.unsent.drain(..n);
            // Once a byte of the last request leaves, all of it must.
            self.owed = self.owed.checked_sub(n).unwrap_or(self.unsent.len());
        }
        Ok(())
    }

    /// Reads until a whole reply to `transaction` is at the front of `received`, and
    /// gives its unit and length. It drops the replies to other requests before it,
    /// the last exchange's included.
    async fn receive(
        &mut self,
        transaction: u16,
        deadline: Option<Monotonic>,
    ) -> Result<(u8, usize), Failure> {
        let mut buffer = [0; 260];
        loop {
            match super::decode(&self.received).map_err(Failure::Stream)? {
                Some(frame) if frame.header.transaction == transaction => {
                    return Ok((frame.header.unit, frame.len));
                }
                Some(frame) => {
                    let len = frame.len;
                    self.received.drain(..len);
                }
                None => {
                    let read = within(&self.clock, deadline, |cx| {
                        self.stream.poll_read(cx, &mut buffer)
                    });
                    let n = read.await.ok_or(Failure::Timeout)??;
                    if n == 0 {
                        return Err(Failure::Closed);
                    }
                    self.received.extend(buffer.iter().take(n));
                }
            }
        }
    }
}

/// Why an exchange gave no reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The stream failed.
    Net(net::Error),
    /// The device closed the stream.
    Closed,
    /// A header that is not valid ([`Error::Protocol`] or [`Error::Length`]). The
    /// stream is out of step.
    Stream(Error),
    /// No whole reply came within the timeout.
    Timeout,
    /// A reply that does not match the request.
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

impl From<net::Error> for Failure {
    fn from(error: net::Error) -> Self {
        Self::Net(error)
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Net(error) => error.fmt(f),
            Self::Closed => write!(f, "the device closed the stream"),
            Self::Timeout => write!(f, "no whole reply came before the timeout"),
            Self::Stream(error) | Self::Frame(error) | Self::Request(error) => {
                error.fmt(f)
            }
            Self::Unit { want, got } => {
                write!(f, "a reply from unit {got} to a request to unit {want}")
            }
        }
    }
}

impl std::error::Error for Failure {}
