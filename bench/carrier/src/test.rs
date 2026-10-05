//! The tests, both sides. A stream starts with one opcode byte. Data then moves in
//! frames: a little-endian `u32` length and that many bytes. Length `END` ends the
//! stream and the server answers it; length `MARK` ends the warmup of a bulk flow. On
//! a `MUX` stream, a tag byte before each frame names its flow, so one TLS connection
//! carries an echo flow and a bulk flow, as the TCP adapter will.

use std::cell::Cell;
use std::collections::VecDeque;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Interval, MissedTickBehavior, interval, timeout};

use crate::cpu::Sample;
use crate::session::{Carrier, Datagrams, Reader, Session, Writer};
use crate::{Error, report};

const BULK: u8 = b'B';
const ECHO: u8 = b'E';
const MUX: u8 = b'M';
const END: u32 = 0;
const MARK: u32 = u32::MAX;
/// Payload bytes per bulk frame, like a large batched frame.
const CHUNK: usize = 64 * 1024;
/// Payload bytes per bulk frame on a `MUX` stream: one TLS record, so an echo frame
/// waits behind at most one bulk frame.
const MUX_CHUNK: usize = 16 * 1024;
/// The largest echo frame.
const MAX_FRAME: usize = 64 * 1024;
/// An echo payload holds its sequence number and send time.
const MIN_FRAME: usize = 16;
/// The first second of each run is not measured: handshake, slow start, MTU search.
const WARMUP: Duration = Duration::from_secs(1);
/// A datagram with no echo after this long counts as lost.
const LOST_AFTER: Duration = Duration::from_millis(100);
/// How long a flow runs on past its `secs` to measure once.
const GRACE: Duration = Duration::from_secs(5);
/// Marks a CPU time the server could not read.
const NONE: u64 = u64::MAX;

/// Where latency frames go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Frames {
    Stream,
    /// QUIC only.
    Datagram,
}

impl FromStr for Frames {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "stream" => Ok(Self::Stream),
            "datagram" => Ok(Self::Datagram),
            _ => Err(format!("unknown frames {s:?}").into()),
        }
    }
}

impl fmt::Display for Frames {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stream => "stream",
            Self::Datagram => "datagram",
        })
    }
}

/// What else runs in the same session during a paced test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Load {
    None,
    /// A bulk flow at a lower priority, on the same connection and thread.
    Shared,
}

impl FromStr for Load {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Error> {
        match s {
            "none" => Ok(Self::None),
            "shared" => Ok(Self::Shared),
            _ => Err(format!("unknown load {s:?}").into()),
        }
    }
}

/// A test measures for `secs` after the [`WARMUP`], and runs on until each of its flows
/// has measured once, so a run starved for all of `secs` still has a result. A flow
/// with no echo by the [`GRACE`] after that fails.
#[derive(Debug)]
pub(crate) enum Test {
    /// One stream, as fast as it goes.
    Bulk { secs: Duration },
    /// One frame in flight, echoed.
    Ping {
        frames: Frames,
        size: usize,
        secs: Duration,
    },
    /// Frames at a fixed rate, echoed.
    Paced {
        frames: Frames,
        size: usize,
        rate: u32,
        secs: Duration,
        load: Load,
    },
}

impl Test {
    /// Parses a test and returns the arguments after it.
    pub(crate) fn parse<'a>(
        args: &'a [&'a str],
    ) -> Result<(Self, &'a [&'a str]), Error> {
        let secs = |s: &str| -> Result<Duration, Error> {
            Ok(Duration::from_secs(s.parse()?))
        };
        let size = |s: &str| -> Result<usize, Error> {
            let size = s.parse()?;
            if !(MIN_FRAME..=MAX_FRAME).contains(&size) {
                let range = format!("{MIN_FRAME}..={MAX_FRAME}");
                return Err(format!("frame size {size} is not in {range}").into());
            }
            Ok(size)
        };
        match args {
            ["bulk", s, rest @ ..] => Ok((Self::Bulk { secs: secs(s)? }, rest)),
            ["ping", frames, n, s, rest @ ..] => {
                let test = Self::Ping {
                    frames: frames.parse()?,
                    size: size(n)?,
                    secs: secs(s)?,
                };
                Ok((test, rest))
            }
            ["paced", frames, n, rate, s, load, rest @ ..] => {
                let rate = rate.parse()?;
                if rate == 0 {
                    return Err("a paced rate is above zero".into());
                }
                let test = Self::Paced {
                    frames: frames.parse()?,
                    size: size(n)?,
                    rate,
                    secs: secs(s)?,
                    load: load.parse()?,
                };
                Ok((test, rest))
            }
            _ => Err(format!("unknown test {args:?}").into()),
        }
    }
}

/// Runs one test on the client.
pub(crate) async fn run(
    session: &mut Session,
    test: &Test,
    cpus: &[usize],
) -> Result<Outcome, Error> {
    let carrier = session.carrier();
    match *test {
        Test::Bulk { secs } => {
            let before = session.stats().map(|s| s.lost_packets);
            let bulk = pump(session.open(0).await?, secs, false, cpus).await?;
            let after = session.stats().map(|s| s.lost_packets);
            let lost = after
                .zip(before)
                .map(|(after, before)| grown(after, before));
            Ok(Outcome::Bulk {
                carrier,
                bulk,
                lost,
            })
        }
        Test::Ping { frames, size, secs } => {
            let (echoes, lost) = match flow(session, frames).await? {
                Flow::Stream(stream) => (ping_stream(stream, size, secs).await?, 0),
                Flow::Datagrams(datagrams) => {
                    ping_datagrams(&datagrams, size, secs).await?
                }
            };
            let label = Label {
                carrier,
                frames,
                test: "ping",
                size,
                rate: None,
            };
            Ok(Outcome::Latency {
                label,
                echoes,
                lost,
                load: None,
            })
        }
        Test::Paced {
            frames,
            size,
            rate,
            secs,
            load,
        } => {
            let pace = Pace {
                size,
                period: Duration::from_secs(1) / rate,
                secs,
            };
            let (echoes, lost, load) = paced(session, frames, pace, load, cpus).await?;
            let label = Label {
                carrier,
                frames,
                test: "paced",
                size,
                rate: Some(rate),
            };
            Ok(Outcome::Latency {
                label,
                echoes,
                lost,
                load,
            })
        }
    }
}

/// The path of latency frames.
enum Flow {
    Stream((Reader, Writer)),
    Datagrams(Datagrams),
}

async fn flow(session: &mut Session, frames: Frames) -> Result<Flow, Error> {
    match frames {
        Frames::Stream => Ok(Flow::Stream(session.open(1).await?)),
        Frames::Datagram => session
            .datagrams()
            .map(Flow::Datagrams)
            .ok_or_else(|| "TLS has no datagrams".into()),
    }
}

/// The frames of a paced flow.
#[derive(Clone, Copy)]
struct Pace {
    size: usize,
    period: Duration,
    secs: Duration,
}

/// Runs a paced flow, with the bulk flow of a `shared` load beside it. Returns the
/// echoes, the frames lost, and the bulk flow.
async fn paced(
    session: &mut Session,
    frames: Frames,
    pace: Pace,
    load: Load,
    cpus: &[usize],
) -> Result<(Vec<Echo>, u64, Option<Bulk>), Error> {
    if session.carrier() == Carrier::Tls
        && frames == Frames::Stream
        && load == Load::Shared
    {
        let (echoes, bulk) = paced_mux(session.open(0).await?, pace, cpus).await?;
        return Ok((echoes, 0, Some(bulk)));
    }
    let flow = flow(session, frames).await?;
    let bulk = match load {
        Load::Shared => Some(session.open(0).await?),
        Load::None => None,
    };
    let latency = async {
        match flow {
            Flow::Stream(stream) => {
                Ok::<_, Error>((paced_stream(stream, pace).await?, 0))
            }
            Flow::Datagrams(datagrams) => paced_datagrams(&datagrams, pace).await,
        }
    };
    let load = async {
        match bulk {
            Some(stream) => {
                Ok::<_, Error>(Some(pump(stream, pace.secs, true, cpus).await?))
            }
            None => Ok(None),
        }
    };
    let ((echoes, lost), load) = tokio::try_join!(latency, load)?;
    Ok((echoes, lost, load))
}

/// Serves one session on the server until the client closes it.
pub(crate) async fn serve(
    mut session: Session,
    cpus: Arc<[usize]>,
) -> Result<(), Error> {
    if let Some(datagrams) = session.datagrams() {
        tokio::spawn(report("datagram echo", echo_datagrams(datagrams)));
    }
    while let Some((r, w)) = session.accept().await? {
        tokio::spawn(report("stream", handle(r, w, Arc::clone(&cpus))));
    }
    Ok(())
}

async fn echo_datagrams(datagrams: Datagrams) -> Result<(), Error> {
    while let Some(datagram) = datagrams.read().await? {
        datagrams.send(datagram)?;
    }
    Ok(())
}

/// Serves one stream on the server.
async fn handle(mut r: Reader, mut w: Writer, cpus: Arc<[usize]>) -> Result<(), Error> {
    let mut buf = vec![0u8; 1 + 4 + MAX_FRAME];
    match r.read_u8().await? {
        BULK => {
            let mut sink = Sink::new(&cpus)?;
            loop {
                match read_frame(&mut r, &mut buf).await? {
                    Frame::End => break,
                    Frame::Mark => sink.mark()?,
                    Frame::Data(len) => sink.add(len),
                }
            }
            w.write_all(&sink.count()?).await?;
        }
        ECHO => loop {
            match read_frame(&mut r, &mut buf).await? {
                Frame::End => {
                    w.write_all(&END.to_le_bytes()).await?;
                    break;
                }
                Frame::Mark => return Err("a mark on an echo stream".into()),
                Frame::Data(len) => {
                    w.write_all(&buf[..4 + len]).await?;
                    w.flush().await?;
                }
            }
        },
        MUX => {
            let mut sink = Sink::new(&cpus)?;
            loop {
                let tag = r.read_u8().await?;
                buf[0] = tag;
                match (tag, read_frame(&mut r, &mut buf[1..]).await?) {
                    (ECHO, Frame::Data(len)) => {
                        w.write_all(&buf[..1 + 4 + len]).await?;
                        w.flush().await?;
                    }
                    (BULK, Frame::Data(len)) => sink.add(len),
                    (BULK, Frame::Mark) => sink.mark()?,
                    (BULK, Frame::End) => break,
                    (tag, frame) => {
                        return Err(format!("{frame:?} with tag {tag}").into());
                    }
                }
            }
            w.write_all(&[BULK]).await?;
            w.write_all(&sink.count()?).await?;
        }
        op => return Err(format!("unknown opcode {op}").into()),
    }
    w.flush().await?;
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum Frame {
    End,
    Mark,
    Data(usize),
}

/// Reads one frame into `buf`: the length, then the payload after it.
async fn read_frame(r: &mut Reader, buf: &mut [u8]) -> Result<Frame, Error> {
    r.read_exact(&mut buf[..4]).await?;
    match u32::from_le_bytes(buf[..4].try_into()?) {
        END => Ok(Frame::End),
        MARK => Ok(Frame::Mark),
        len => {
            let len = usize::try_from(len)?;
            if len > MAX_FRAME {
                return Err(format!("frame of {len} bytes").into());
            }
            r.read_exact(&mut buf[4..4 + len]).await?;
            Ok(Frame::Data(len))
        }
    }
}

/// A frame: `tag` (empty or one byte), length, and payload.
fn frame(tag: &[u8], payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len()).expect("frames are at most 64 KiB");
    [tag, &len.to_le_bytes(), payload].concat()
}

/// A tagged frame with no payload.
fn head(tag: u8, len: u32) -> Vec<u8> {
    [&[tag][..], &len.to_le_bytes()].concat()
}

/// The server's side of a bulk flow: bytes and CPU time after the warmup mark.
struct Sink<'a> {
    cpus: &'a [usize],
    bytes: u64,
    start: Sample,
}

impl<'a> Sink<'a> {
    fn new(cpus: &'a [usize]) -> Result<Self, Error> {
        Ok(Self {
            cpus,
            bytes: 0,
            start: Sample::now(cpus)?,
        })
    }

    fn mark(&mut self) -> Result<(), Error> {
        self.bytes = 0;
        self.start = Sample::now(self.cpus)?;
        Ok(())
    }

    fn add(&mut self, len: usize) {
        self.bytes += u64::try_from(len).expect("a frame length fits in u64");
    }

    /// The answer to the client: `END`, bytes, thread time, and CPU time.
    fn count(&self) -> Result<Vec<u8>, Error> {
        Ok(count(self.bytes, Sample::now(self.cpus)?.since(self.start)))
    }
}

fn count(bytes: u64, used: Sample) -> Vec<u8> {
    let mut count = END.to_le_bytes().to_vec();
    for n in [
        bytes,
        used.thread.unwrap_or(NONE),
        used.cpus.unwrap_or(NONE),
    ] {
        count.extend_from_slice(&n.to_le_bytes());
    }
    count
}

/// Reads the server's answer to a bulk flow and checks its byte count.
async fn read_count(r: &mut Reader, sent: u64) -> Result<Sample, Error> {
    let mut count = [0u8; 4 + 3 * 8];
    r.read_exact(&mut count).await?;
    let counted = word(&count, 4);
    if count[..4] != END.to_le_bytes() || counted != sent {
        return Err(format!("sent {sent} bytes, server counted {counted}").into());
    }
    let known = |n: u64| (n != NONE).then_some(n);
    Ok(Sample {
        thread: known(word(&count, 12)),
        cpus: known(word(&count, 20)),
    })
}

/// The little-endian `u64` at `at`.
fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

pub(crate) struct Bulk {
    bytes: u64,
    elapsed: Duration,
    client: Sample,
    server: Sample,
}

impl Bulk {
    fn gbps(&self) -> f64 {
        float(self.bytes) * 8.0 / self.elapsed.as_secs_f64() / 1e9
    }
}

/// Sends bulk frames for the warmup and `secs`, then waits for the server's count.
/// A `yielding` pump lets the other tasks on its thread run after each frame: a QUIC
/// stream write rarely waits, so without it a bulk flow starves a paced one.
async fn pump(
    (mut r, mut w): (Reader, Writer),
    secs: Duration,
    yielding: bool,
    cpus: &[usize],
) -> Result<Bulk, Error> {
    w.write_all(&[BULK]).await?;
    let bulk = frame(&[], &vec![0xa5; CHUNK]);
    send_until(&mut w, &bulk, Instant::now() + WARMUP, yielding).await?;
    w.write_all(&MARK.to_le_bytes()).await?;
    let cpu = Sample::now(cpus)?;
    let begin = Instant::now();
    let frames = send_until(&mut w, &bulk, begin + secs, yielding).await?;
    w.write_all(&END.to_le_bytes()).await?;
    w.flush().await?;
    let bytes = frames * u64::try_from(CHUNK)?;
    let server = read_count(&mut r, bytes).await?;
    Ok(Bulk {
        bytes,
        elapsed: begin.elapsed(),
        client: Sample::now(cpus)?.since(cpu),
        server,
    })
}

/// Writes `frame` once, then until `end`, and returns how many it wrote.
async fn send_until(
    w: &mut Writer,
    frame: &[u8],
    end: Instant,
    yielding: bool,
) -> Result<u64, Error> {
    let mut frames = 0;
    loop {
        w.write_all(frame).await?;
        frames += 1;
        if yielding {
            tokio::task::yield_now().await;
        }
        if Instant::now() >= end {
            return Ok(frames);
        }
    }
}

/// One measured echo, in nanoseconds: the round trip from the send, and for a paced
/// frame, how long after its due time it was sent.
pub(crate) struct Echo {
    rtt: u64,
    delay: Option<u64>,
}

/// An echo payload: sequence number and send time, padded to `size`.
fn payload(size: usize, seq: u64, sent: Duration) -> Vec<u8> {
    let mut payload = vec![0u8; size];
    payload[..8].copy_from_slice(&seq.to_le_bytes());
    payload[8..16].copy_from_slice(&nanos(sent).to_le_bytes());
    payload
}

/// Reads the sequence number and send time from an echo payload.
fn stamp(payload: &[u8]) -> (u64, Duration) {
    (word(payload, 0), Duration::from_nanos(word(payload, 8)))
}

/// The sequence number and round trip of an echo payload that was sent after the
/// warmup.
fn measured(base: Instant, payload: &[u8]) -> Option<(u64, Duration)> {
    let (seq, sent) = stamp(payload);
    (sent >= WARMUP).then(|| (seq, rtt(base, sent)))
}

/// The time from `sent` after `base` to now.
fn rtt(base: Instant, sent: Duration) -> Duration {
    base.elapsed()
        .checked_sub(sent)
        .expect("invariant: an echo arrives after its send")
}

async fn ping_stream(
    (mut r, mut w): (Reader, Writer),
    size: usize,
    secs: Duration,
) -> Result<Vec<Echo>, Error> {
    w.write_all(&[ECHO]).await?;
    let base = Instant::now();
    let mut back = vec![0u8; 4 + size];
    let mut echoes = Vec::new();
    for seq in 0.. {
        let since = base.elapsed();
        if ended(since, secs, !echoes.is_empty())? {
            break;
        }
        w.write_all(&frame(&[], &payload(size, seq, since))).await?;
        w.flush().await?;
        r.read_exact(&mut back).await?;
        echoes.extend(measured(base, &back[4..]).map(|(_, rtt)| Echo {
            rtt: nanos(rtt),
            delay: None,
        }));
    }
    w.write_all(&END.to_le_bytes()).await?;
    w.flush().await?;
    r.read_exact(&mut back[..4]).await?;
    Ok(echoes)
}

async fn ping_datagrams(
    datagrams: &Datagrams,
    size: usize,
    secs: Duration,
) -> Result<(Vec<Echo>, u64), Error> {
    let base = Instant::now();
    let (mut echoes, mut lost) = (Vec::new(), 0);
    for seq in 0.. {
        let since = base.elapsed();
        if ended(since, secs, !echoes.is_empty())? {
            break;
        }
        datagrams.send(payload(size, seq, since))?;
        let echo = timeout(LOST_AFTER, async {
            loop {
                let back = datagrams.read().await?.ok_or("the session closed")?;
                if stamp(&back).0 == seq {
                    return Ok::<_, Error>(back);
                }
            }
        });
        match echo.await {
            Ok(back) => echoes.extend(measured(base, &back?).map(|(_, rtt)| Echo {
                rtt: nanos(rtt),
                delay: None,
            })),
            Err(_) if since >= WARMUP => lost += 1,
            Err(_) => {}
        }
    }
    Ok((echoes, lost))
}

/// Whether a flow is over at `since` from its start: `secs` after the warmup passed,
/// and the flow has `measured` once. Fails when it measured nothing by the [`GRACE`]
/// after that.
fn ended(since: Duration, secs: Duration, measured: bool) -> Result<bool, Error> {
    let end = WARMUP + secs;
    if !measured && since >= end + GRACE {
        return Err(format!("no echo in the {GRACE:?} after the run").into());
    }
    Ok(measured && since >= end)
}

/// The send times of a paced flow: one frame each period from `base`.
struct Schedule {
    base: Instant,
    tick: Interval,
    /// How late each frame was sent, in nanoseconds, by sequence number.
    delays: Vec<u64>,
}

impl Schedule {
    fn new(period: Duration) -> Self {
        let mut tick = interval(period);
        // Missed frames go out at once, each with its own due time.
        tick.set_missed_tick_behavior(MissedTickBehavior::Burst);
        Self {
            base: Instant::now(),
            tick,
            delays: Vec::new(),
        }
    }

    /// Waits until the next frame is due. Returns its sequence number and send time
    /// from `base`. Cancel safe.
    async fn next(&mut self) -> (u64, Duration) {
        let due = self.tick.tick().await.into_std();
        let sent = Instant::now();
        let late = sent
            .checked_duration_since(due)
            .expect("invariant: a tick fires at or after its due time");
        let seq = u64::try_from(self.delays.len()).expect("fewer than 2^64 frames");
        self.delays.push(nanos(late));
        (seq, sent.duration_since(self.base))
    }
}

async fn paced_stream(
    (mut r, mut w): (Reader, Writer),
    pace: Pace,
) -> Result<Vec<Echo>, Error> {
    w.write_all(&[ECHO]).await?;
    let mut schedule = Schedule::new(pace.period);
    let base = schedule.base;
    let echoed = Cell::new(false);
    let send = async {
        loop {
            let (seq, since) = schedule.next().await;
            if ended(since, pace.secs, echoed.get())? {
                break;
            }
            w.write_all(&frame(&[], &payload(pace.size, seq, since)))
                .await?;
            w.flush().await?;
        }
        w.write_all(&END.to_le_bytes()).await?;
        w.flush().await?;
        Ok::<_, Error>(())
    };
    let receive = async {
        let mut back = vec![0u8; 4 + pace.size];
        let mut rtts = Vec::new();
        loop {
            r.read_exact(&mut back[..4]).await?;
            if back[..4] == END.to_le_bytes() {
                return Ok::<_, Error>(rtts);
            }
            r.read_exact(&mut back[4..]).await?;
            rtts.extend(measured(base, &back[4..]));
            echoed.set(!rtts.is_empty());
        }
    };
    let ((), rtts) = tokio::try_join!(send, receive)?;
    paced_echoes(&rtts, &schedule.delays)
}

async fn paced_datagrams(
    datagrams: &Datagrams,
    pace: Pace,
) -> Result<(Vec<Echo>, u64), Error> {
    let mut schedule = Schedule::new(pace.period);
    let base = schedule.base;
    let mut expected = 0;
    let (echoed, done) = (Cell::new(false), Cell::new(false));
    let send = async {
        loop {
            let (seq, since) = schedule.next().await;
            if ended(since, pace.secs, echoed.get())? {
                break;
            }
            if since >= WARMUP {
                expected += 1;
            }
            datagrams.send(payload(pace.size, seq, since))?;
        }
        done.set(true);
        Ok::<_, Error>(())
    };
    let receive = async {
        let mut rtts = Vec::new();
        loop {
            match timeout(LOST_AFTER, datagrams.read()).await {
                Ok(back) => {
                    rtts.extend(measured(base, &back?.ok_or("the session closed")?));
                    echoed.set(!rtts.is_empty());
                }
                Err(_) if done.get() => return Ok::<_, Error>(rtts),
                Err(_) => {}
            }
        }
    };
    let ((), rtts) = tokio::try_join!(send, receive)?;
    let echoed = u64::try_from(rtts.len())?;
    Ok((
        paced_echoes(&rtts, &schedule.delays)?,
        grown(expected, echoed),
    ))
}

/// Paced echo frames and a bulk flow on one TLS stream.
async fn paced_mux(
    (mut r, mut w): (Reader, Writer),
    pace: Pace,
    cpus: &[usize],
) -> Result<(Vec<Echo>, Bulk), Error> {
    w.write_all(&[MUX]).await?;
    let mut schedule = Schedule::new(pace.period);
    let base = schedule.base;
    let echoed = Cell::new(false);
    let send = mux_send(&mut w, &mut schedule, pace, &echoed, cpus);
    let receive = async {
        let mut back = vec![0u8; 4 + pace.size];
        let mut rtts = Vec::new();
        loop {
            match r.read_u8().await? {
                ECHO => {
                    r.read_exact(&mut back).await?;
                    rtts.extend(measured(base, &back[4..]));
                    echoed.set(!rtts.is_empty());
                }
                BULK => return Ok::<_, Error>(rtts),
                tag => return Err(format!("unknown tag {tag}").into()),
            }
        }
    };
    let ((frames, begin, cpu), rtts) = tokio::try_join!(send, receive)?;
    let bytes = frames * u64::try_from(MUX_CHUNK)?;
    let server = read_count(&mut r, bytes).await?;
    let bulk = Bulk {
        bytes,
        elapsed: begin.elapsed(),
        client: Sample::now(cpus)?.since(cpu),
        server,
    };
    Ok((paced_echoes(&rtts, &schedule.delays)?, bulk))
}

/// Sends bulk frames and the due echo frames on a `MUX` stream until the run ends, at
/// the end of `pace` once a bulk frame went out after the warmup mark and `echoed` is
/// set, which the receive half does on an echo of a frame sent after the warmup. A due
/// echo frame goes out after the bulk frame in progress. Returns the bulk frames sent
/// after the mark, and the time and CPU sample at the mark.
async fn mux_send(
    w: &mut Writer,
    schedule: &mut Schedule,
    pace: Pace,
    echoed: &Cell<bool>,
    cpus: &[usize],
) -> Result<(u64, Instant, Sample), Error> {
    let bulk = frame(&[BULK], &vec![0xa5; MUX_CHUNK]);
    let mut queue: VecDeque<Vec<u8>> = VecDeque::new();
    let (mut offset, mut frames, mut mark) = (0, 0, None);
    loop {
        if offset == 0 {
            for echo in queue.drain(..) {
                w.write_all(&echo).await?;
            }
            let since = schedule.base.elapsed();
            if mark.is_none() && since >= WARMUP {
                w.write_all(&head(BULK, MARK)).await?;
                mark = Some((Instant::now(), Sample::now(cpus)?));
            }
            if ended(since, pace.secs, frames > 0 && echoed.get())? {
                break;
            }
        }
        tokio::select! {
            biased;
            (seq, since) = schedule.next() => {
                queue.push_back(frame(&[ECHO], &payload(pace.size, seq, since)));
            }
            written = w.write(&bulk[offset..]) => {
                match written? {
                    0 => return Err("the stream closed".into()),
                    n => offset += n,
                }
                if offset == bulk.len() {
                    offset = 0;
                    if mark.is_some() {
                        frames += 1;
                    }
                }
            }
        }
    }
    w.write_all(&head(BULK, END)).await?;
    w.flush().await?;
    let (begin, cpu) = mark.expect("invariant: frames are counted after the mark");
    Ok((frames, begin, cpu))
}

/// Joins each round trip with the send delay of its frame.
fn paced_echoes(rtts: &[(u64, Duration)], delays: &[u64]) -> Result<Vec<Echo>, Error> {
    rtts.iter()
        .map(|&(seq, rtt)| {
            let delay = usize::try_from(seq)
                .ok()
                .and_then(|seq| delays.get(seq))
                .ok_or_else(|| format!("an echo of frame {seq}, which was not sent"))?;
            Ok(Echo {
                rtt: nanos(rtt),
                delay: Some(*delay),
            })
        })
        .collect()
}

/// The header of [`Outcome::line`] for a bulk test.
pub(crate) const BULK_COLUMNS: &str = "| carrier | Gbit/s | lost packets | client core % \
| client thread ns/B | client CPUs ns/B | server core % | server thread ns/B \
| server CPUs ns/B |";

/// The header of [`Outcome::line`] for a ping or paced test, in microseconds.
pub(crate) const LATENCY_COLUMNS: &str = "| carrier | frames | test | size | rate \
| load Gbit/s | n | lost | p50 us | p99 us | p99.9 us | max us | delay p99 us \
| delay max us |";

/// What the client measured.
pub(crate) enum Outcome {
    Bulk {
        carrier: Carrier,
        bulk: Bulk,
        /// QUIC packets lost; TLS has no count.
        lost: Option<u64>,
    },
    Latency {
        label: Label,
        echoes: Vec<Echo>,
        /// Datagrams with no echo.
        lost: u64,
        /// The bulk flow of a `shared` load.
        load: Option<Bulk>,
    },
}

impl Outcome {
    /// The Markdown table line.
    pub(crate) fn line(&self) -> String {
        match self {
            Self::Bulk {
                carrier,
                bulk,
                lost,
            } => {
                let per_byte = |ns: Option<u64>| {
                    ns.map_or("-".into(), |ns| {
                        format!("{:.3}", float(ns) / float(bulk.bytes))
                    })
                };
                let share = |ns: Option<u64>| {
                    let secs = bulk.elapsed.as_secs_f64();
                    ns.map_or("-".into(), |ns| {
                        format!("{:.0}%", float(ns) / secs / 1e7)
                    })
                };
                format!(
                    "| {carrier} | {:.2} | {} | {} | {} | {} | {} | {} | {} |",
                    bulk.gbps(),
                    lost.map_or("-".into(), |n| n.to_string()),
                    share(bulk.client.thread),
                    per_byte(bulk.client.thread),
                    per_byte(bulk.client.cpus),
                    share(bulk.server.thread),
                    per_byte(bulk.server.thread),
                    per_byte(bulk.server.cpus),
                )
            }
            Self::Latency {
                label,
                echoes,
                lost,
                load,
            } => {
                let mut rtts: Vec<_> = echoes.iter().map(|e| e.rtt).collect();
                let mut delays: Vec<_> =
                    echoes.iter().filter_map(|e| e.delay).collect();
                rtts.sort_unstable();
                delays.sort_unstable();
                let load = load
                    .as_ref()
                    .map_or("-".into(), |b| format!("{:.2}", b.gbps()));
                format!(
                    "| {label} | {load} | {} | {lost} | {} | {} | {} | {} | {} | {} |",
                    rtts.len(),
                    micros(&rtts, 500),
                    micros(&rtts, 990),
                    micros(&rtts, 999),
                    micros(&rtts, 1000),
                    micros(&delays, 990),
                    micros(&delays, 1000),
                )
            }
        }
    }

    /// Each measured echo as `rtt_ns delay_ns`, one per line, for pooling across
    /// repetitions.
    pub(crate) fn samples(&self) -> String {
        let Self::Latency { echoes, .. } = self else {
            return String::new();
        };
        echoes
            .iter()
            .map(|e| match e.delay {
                Some(delay) => format!("{} {delay}\n", e.rtt),
                None => format!("{} -\n", e.rtt),
            })
            .collect()
    }
}

pub(crate) struct Label {
    carrier: Carrier,
    frames: Frames,
    test: &'static str,
    size: usize,
    rate: Option<u32>,
}

impl fmt::Display for Label {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rate = self.rate.map_or("-".into(), |r| r.to_string());
        write!(
            f,
            "{} | {} | {} | {} | {rate}",
            self.carrier, self.frames, self.test, self.size
        )
    }
}

/// The nearest-rank percentile of sorted nanoseconds, in microseconds.
fn micros(sorted: &[u64], per_mille: usize) -> String {
    if sorted.is_empty() {
        return "-".into();
    }
    let rank = (sorted.len() * per_mille)
        .div_ceil(1000)
        .clamp(1, sorted.len());
    format!("{:.1}", float(sorted[rank - 1]) / 1e3)
}

/// `after - before` for a count that only grows.
fn grown(after: u64, before: u64) -> u64 {
    after
        .checked_sub(before)
        .expect("invariant: a count never goes back")
}

#[expect(
    clippy::cast_precision_loss,
    reason = "printed figures need far fewer than 53 bits"
)]
fn float(n: u64) -> f64 {
    n as f64
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).expect("under 584 years")
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::session::{Config, Listener};

    const WINDOW: Duration = Duration::from_millis(100);

    /// Runs `test` against a loopback server that echoes.
    async fn loopback(carrier: Carrier, test: Test) -> Result<Outcome, Error> {
        against(carrier, test, |listener| {
            crate::serve(listener, Arc::from([]))
        })
        .await
    }

    /// Runs `test` over QUIC against a loopback server that holds the session and
    /// echoes nothing.
    async fn silent(test: Test) -> Result<Outcome, Error> {
        against(Carrier::Quic, test, |listener| async move {
            let _session = listener.accept().await?.finish().await?;
            std::future::pending().await
        })
        .await
    }

    async fn against<F>(
        carrier: Carrier,
        test: Test,
        serve: impl FnOnce(Listener) -> F,
    ) -> Result<Outcome, Error>
    where
        F: Future<Output = Result<(), Error>> + Send + 'static,
    {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata");
        let provider = Arc::new(Config::provider());
        let config = Config::new(carrier, provider, false, 1500)?;
        let (cert, key) = (format!("{dir}/cert.pem"), format!("{dir}/key.pem"));
        let listener =
            Listener::bind(&config, "127.0.0.1:0".parse()?, &cert, &key).await?;
        let server = listener.local_addr()?;
        let serving = tokio::spawn(serve(listener));
        let mut session =
            Session::connect(&config, server, &format!("{dir}/ca.pem")).await?;
        // A broken protocol waits forever; this makes it fail.
        let outcome =
            timeout(Duration::from_secs(10), run(&mut session, &test, &[])).await?;
        session.close().await;
        serving.abort();
        outcome
    }

    fn assert_bulk(outcome: &Outcome) {
        let Outcome::Bulk { bulk, .. } = outcome else {
            panic!("not a bulk outcome");
        };
        assert!(bulk.bytes > 0, "the server counted no bytes");
    }

    /// Asserts measured echoes, a send delay for each paced one, and the bulk flow of
    /// a shared load.
    fn assert_latency(outcome: &Outcome, paced: bool, shared: bool) {
        let Outcome::Latency { echoes, load, .. } = outcome else {
            panic!("not a latency outcome");
        };
        assert!(!echoes.is_empty(), "no echo was measured");
        assert!(
            echoes.iter().all(|e| e.delay.is_some() == paced),
            "wrong delays"
        );
        assert_eq!(load.as_ref().map(|b| b.bytes > 0), shared.then_some(true));
    }

    #[tokio::test]
    async fn bulk_counts_match_over_quic() {
        assert_bulk(
            &loopback(Carrier::Quic, Test::Bulk { secs: WINDOW })
                .await
                .unwrap(),
        );
    }

    #[tokio::test]
    async fn bulk_counts_match_over_tls() {
        assert_bulk(
            &loopback(Carrier::Tls, Test::Bulk { secs: WINDOW })
                .await
                .unwrap(),
        );
    }

    #[tokio::test]
    async fn a_bulk_run_of_zero_seconds_counts_a_frame() {
        let test = Test::Bulk {
            secs: Duration::ZERO,
        };
        assert_bulk(&loopback(Carrier::Quic, test).await.unwrap());
    }

    fn ping(frames: Frames, secs: Duration) -> Test {
        Test::Ping {
            frames,
            size: 64,
            secs,
        }
    }

    fn paced(frames: Frames, secs: Duration) -> Test {
        Test::Paced {
            frames,
            size: 256,
            rate: 1000,
            secs,
            load: Load::Shared,
        }
    }

    #[tokio::test]
    async fn ping_echoes_quic_streams() {
        let outcome = loopback(Carrier::Quic, ping(Frames::Stream, WINDOW))
            .await
            .unwrap();
        assert_latency(&outcome, false, false);
    }

    #[tokio::test]
    async fn ping_echoes_quic_datagrams() {
        let outcome = loopback(Carrier::Quic, ping(Frames::Datagram, WINDOW))
            .await
            .unwrap();
        assert_latency(&outcome, false, false);
    }

    #[tokio::test]
    async fn ping_echoes_the_tls_stream() {
        let outcome = loopback(Carrier::Tls, ping(Frames::Stream, WINDOW))
            .await
            .unwrap();
        assert_latency(&outcome, false, false);
    }

    #[tokio::test]
    async fn a_ping_run_of_zero_seconds_measures_a_stream_echo() {
        let test = ping(Frames::Stream, Duration::ZERO);
        let outcome = loopback(Carrier::Quic, test).await.unwrap();
        assert_latency(&outcome, false, false);
    }

    #[tokio::test]
    async fn a_ping_run_of_zero_seconds_measures_a_datagram_echo() {
        let test = ping(Frames::Datagram, Duration::ZERO);
        let outcome = loopback(Carrier::Quic, test).await.unwrap();
        assert_latency(&outcome, false, false);
    }

    /// Asserts the error of a run that measured no echo.
    fn assert_no_echo(outcome: Result<Outcome, Error>) {
        let error = outcome.err().unwrap().to_string();
        assert_eq!(error, "no echo in the 5s after the run");
    }

    #[tokio::test]
    async fn a_datagram_ping_with_no_echo_fails_after_the_grace() {
        assert_no_echo(silent(ping(Frames::Datagram, Duration::ZERO)).await);
    }

    #[tokio::test]
    async fn a_paced_datagram_run_with_no_echo_fails_after_the_grace() {
        let test = Test::Paced {
            frames: Frames::Datagram,
            size: 256,
            rate: 1000,
            secs: Duration::ZERO,
            load: Load::None,
        };
        assert_no_echo(silent(test).await);
    }

    #[tokio::test]
    async fn ping_rejects_datagrams_over_tls() {
        let error = loopback(Carrier::Tls, ping(Frames::Datagram, WINDOW))
            .await
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "TLS has no datagrams");
    }

    #[tokio::test]
    async fn paced_shares_a_quic_connection_with_bulk_streams() {
        let outcome = loopback(Carrier::Quic, paced(Frames::Stream, WINDOW))
            .await
            .unwrap();
        assert_latency(&outcome, true, true);
    }

    #[tokio::test]
    async fn paced_shares_a_quic_connection_with_bulk_datagrams() {
        let outcome = loopback(Carrier::Quic, paced(Frames::Datagram, WINDOW))
            .await
            .unwrap();
        assert_latency(&outcome, true, true);
    }

    #[tokio::test]
    async fn paced_multiplexes_bulk_on_the_tls_stream() {
        let outcome = loopback(Carrier::Tls, paced(Frames::Stream, WINDOW))
            .await
            .unwrap();
        assert_latency(&outcome, true, true);
    }

    #[tokio::test]
    async fn a_paced_run_of_zero_seconds_measures_a_quic_stream_echo_and_bulk() {
        let test = paced(Frames::Stream, Duration::ZERO);
        let outcome = loopback(Carrier::Quic, test).await.unwrap();
        assert_latency(&outcome, true, true);
    }

    #[tokio::test]
    async fn a_paced_run_of_zero_seconds_measures_a_datagram_echo_and_bulk() {
        let test = paced(Frames::Datagram, Duration::ZERO);
        let outcome = loopback(Carrier::Quic, test).await.unwrap();
        assert_latency(&outcome, true, true);
    }

    #[tokio::test]
    async fn a_paced_run_of_zero_seconds_measures_a_tls_echo_and_bulk() {
        let test = paced(Frames::Stream, Duration::ZERO);
        let outcome = loopback(Carrier::Tls, test).await.unwrap();
        assert_latency(&outcome, true, true);
    }

    #[test]
    fn columns_match_the_lines() {
        let bulk = Outcome::Bulk {
            carrier: Carrier::Tls,
            bulk: Bulk {
                bytes: 1,
                elapsed: Duration::from_secs(1),
                client: Sample {
                    thread: None,
                    cpus: None,
                },
                server: Sample {
                    thread: None,
                    cpus: None,
                },
            },
            lost: None,
        };
        let pipes = |line: &str| line.matches('|').count();
        assert_eq!(pipes(&bulk.line()), pipes(BULK_COLUMNS));
        let latency = Outcome::Latency {
            label: Label {
                carrier: Carrier::Quic,
                frames: Frames::Stream,
                test: "ping",
                size: 64,
                rate: None,
            },
            echoes: Vec::new(),
            lost: 0,
            load: None,
        };
        assert_eq!(pipes(&latency.line()), pipes(LATENCY_COLUMNS));
    }

    fn reader(bytes: Vec<u8>) -> Reader {
        Box::new(Cursor::new(bytes))
    }

    fn parse_error(args: &[&str]) -> String {
        Test::parse(args)
            .expect_err("the test is invalid")
            .to_string()
    }

    #[test]
    fn parse_returns_the_options_after_the_test() {
        let (test, rest) = Test::parse(&["bulk", "20", "unsegmented"]).unwrap();
        assert!(matches!(test, Test::Bulk { secs } if secs == Duration::from_secs(20)));
        assert_eq!(rest, ["unsegmented"]);
    }

    #[test]
    fn parse_rejects_a_frame_too_small_for_its_stamp() {
        let error = parse_error(&["ping", "stream", "8", "1"]);
        assert_eq!(error, "frame size 8 is not in 16..=65536");
    }

    #[test]
    fn parse_rejects_a_frame_larger_than_the_server_reads() {
        let error = parse_error(&["ping", "stream", "70000", "1"]);
        assert_eq!(error, "frame size 70000 is not in 16..=65536");
    }

    #[test]
    fn parse_rejects_a_zero_rate() {
        let error = parse_error(&["paced", "stream", "256", "0", "1", "none"]);
        assert_eq!(error, "a paced rate is above zero");
    }

    #[test]
    fn parse_rejects_an_unknown_load() {
        let error = parse_error(&["paced", "stream", "256", "1000", "1", "lod"]);
        assert_eq!(error, r#"unknown load "lod""#);
    }

    #[tokio::test]
    async fn frames_read_back_as_written() {
        let mut bytes = frame(&[], &payload(16, 7, Duration::from_nanos(3)));
        bytes.extend_from_slice(&MARK.to_le_bytes());
        bytes.extend_from_slice(&END.to_le_bytes());
        let mut r = reader(bytes);
        let mut buf = vec![0u8; 4 + MAX_FRAME];
        assert_eq!(read_frame(&mut r, &mut buf).await.unwrap(), Frame::Data(16));
        assert_eq!(stamp(&buf[4..20]), (7, Duration::from_nanos(3)));
        assert_eq!(read_frame(&mut r, &mut buf).await.unwrap(), Frame::Mark);
        assert_eq!(read_frame(&mut r, &mut buf).await.unwrap(), Frame::End);
    }

    #[tokio::test]
    async fn read_frame_rejects_a_frame_larger_than_its_buffer() {
        let len = u32::try_from(MAX_FRAME + 1).unwrap();
        let mut r = reader(len.to_le_bytes().to_vec());
        let mut buf = vec![0u8; 4 + MAX_FRAME];
        let error = read_frame(&mut r, &mut buf).await.unwrap_err();
        assert_eq!(error.to_string(), "frame of 65537 bytes");
    }

    #[tokio::test]
    async fn read_count_returns_the_server_cpu_time() {
        let used = Sample {
            thread: Some(5),
            cpus: None,
        };
        let mut r = reader(count(1 << 20, used));
        let sample = read_count(&mut r, 1 << 20).await.unwrap();
        assert_eq!((sample.thread, sample.cpus), (Some(5), None));
    }

    #[tokio::test]
    async fn read_count_rejects_a_byte_count_that_differs() {
        let used = Sample {
            thread: None,
            cpus: None,
        };
        let mut r = reader(count(5, used));
        let error = read_count(&mut r, 10).await.unwrap_err();
        assert_eq!(error.to_string(), "sent 10 bytes, server counted 5");
    }

    #[test]
    fn micros_takes_the_nearest_rank() {
        let sorted: Vec<u64> = (1..=1000).map(|n| n * 1000).collect();
        let ranks = [500, 990, 999, 1000].map(|p| micros(&sorted, p));
        assert_eq!(ranks, ["500.0", "990.0", "999.0", "1000.0"]);
        assert_eq!(micros(&[42_000], 500), "42.0");
        assert_eq!(micros(&[], 500), "-");
    }

    #[test]
    fn paced_echoes_reject_an_echo_of_a_frame_not_sent() {
        let error = paced_echoes(&[(5, Duration::ZERO)], &[0; 5]).err().unwrap();
        assert_eq!(error.to_string(), "an echo of frame 5, which was not sent");
    }

    #[test]
    fn latency_line_has_the_header_columns() {
        let echoes = (1..=4)
            .map(|n| Echo {
                rtt: n * 1000,
                delay: Some(n * 10),
            })
            .collect();
        let outcome = Outcome::Latency {
            label: Label {
                carrier: Carrier::Quic,
                frames: Frames::Datagram,
                test: "paced",
                size: 256,
                rate: Some(1000),
            },
            echoes,
            lost: 2,
            load: None,
        };
        assert_eq!(
            outcome.line(),
            "| quic | datagram | paced | 256 | 1000 | - | 4 | 2 | 2.0 | 4.0 | 4.0 | 4.0 \
             | 0.0 | 0.0 |"
        );
        assert_eq!(outcome.samples(), "1000 10\n2000 20\n3000 30\n4000 40\n");
    }
}
