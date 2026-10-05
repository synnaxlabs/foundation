//! The tests, both sides. A stream starts with one opcode byte. Data then moves in
//! frames: a little-endian `u32` length and that many bytes. A zero length ends the
//! stream, and the server answers it.

use std::cell::Cell;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{MissedTickBehavior, interval, timeout};

use crate::Error;
use crate::cpu::Sample;
use crate::link::{Link, Reader, Writer};

const BULK: u8 = b'B';
const ECHO: u8 = b'E';
/// Bytes per bulk write, like a large batched frame.
const CHUNK: usize = 64 * 1024;
/// The largest echo frame.
const MAX_FRAME: usize = 64 * 1024;
/// Results from the first second are dropped: handshake, slow start, cold caches.
const WARMUP: Duration = Duration::from_secs(1);
/// A datagram with no echo after this long counts as lost.
const LOST_AFTER: Duration = Duration::from_millis(100);
/// Marks a CPU time the server could not read.
const NONE: u64 = u64::MAX;

pub enum Test {
    Bulk {
        secs: Duration,
    },
    Ping {
        size: usize,
        secs: Duration,
    },
    Paced {
        size: usize,
        rate: u32,
        secs: Duration,
        load: bool,
    },
}

/// Runs one test and returns its Markdown table line.
pub async fn run(link: &Link, test: &Test) -> Result<String, Error> {
    let (carrier, gso) = (link.carrier, gso(link));
    match *test {
        Test::Bulk { secs } => {
            let (r, w) = link.open(0).await?;
            let lost = link.lost_packets();
            let bulk = pump(r, w, secs).await?;
            let lost = link
                .lost_packets()
                .zip(lost)
                .map_or("-".into(), |(b, a)| (b - a).to_string());
            let client = |ns| per_byte(ns, bulk.bytes);
            Ok(format!(
                "| {carrier} | {gso} | {:.2} | {lost} | {} | {} | {} | {} | {} | {} |",
                bulk.bytes as f64 * 8.0 / bulk.elapsed.as_secs_f64() / 1e9,
                share(bulk.client.thread, bulk.elapsed),
                share(bulk.server.thread, bulk.elapsed),
                client(bulk.client.thread),
                client(bulk.client.host),
                client(bulk.server.thread),
                client(bulk.server.host),
            ))
        }
        Test::Ping { size, secs } => {
            let (rtts, lost) = match link.datagrams() {
                Some(conn) => ping_datagrams(conn, size, secs).await?,
                None => (ping_stream(link, size, secs).await?, 0),
            };
            Ok(latency_row(link, "ping", size, "-", rtts, lost))
        }
        Test::Paced {
            size,
            rate,
            secs,
            load,
        } => {
            let period = Duration::from_secs_f64(1.0 / f64::from(rate));
            let latency = async {
                match link.datagrams() {
                    Some(conn) => paced_datagrams(conn, size, period, secs).await,
                    None => Ok((paced_stream(link, size, period, secs).await?, 0)),
                }
            };
            let bulk = async {
                if load {
                    let (r, w) = link.open(0).await?;
                    pump(r, w, WARMUP + secs).await?;
                }
                Ok::<_, Error>(())
            };
            let ((rtts, lost), ()) = tokio::try_join!(latency, bulk)?;
            let rate = rate.to_string();
            Ok(latency_row(link, "paced", size, &rate, rtts, lost))
        }
    }
}

/// Serves one stream on the server.
pub async fn handle(mut r: Reader, mut w: Writer) -> Result<(), Error> {
    let mut buf = vec![0u8; 4 + MAX_FRAME];
    let op = r.read_u8().await?;
    let start = Sample::now();
    let mut bytes = 0u64;
    loop {
        r.read_exact(&mut buf[..4]).await?;
        let len = u32::from_le_bytes(buf[..4].try_into()?) as usize;
        if len > MAX_FRAME {
            return Err(format!("frame of {len} bytes").into());
        }
        if len == 0 {
            break;
        }
        r.read_exact(&mut buf[4..4 + len]).await?;
        bytes += len as u64;
        if op == ECHO {
            w.write_all(&buf[..4 + len]).await?;
            w.flush().await?;
        }
    }
    let used = Sample::now().since(start);
    let mut end = Vec::with_capacity(28);
    end.extend_from_slice(&0u32.to_le_bytes());
    if op == BULK {
        for n in [
            bytes,
            used.thread.unwrap_or(NONE),
            used.host.unwrap_or(NONE),
        ] {
            end.extend_from_slice(&n.to_le_bytes());
        }
    }
    w.write_all(&end).await?;
    w.flush().await?;
    Ok(())
}

struct Bulk {
    bytes: u64,
    elapsed: Duration,
    client: Sample,
    server: Sample,
}

/// Sends bulk frames for `secs`, then waits for the server's count.
async fn pump(mut r: Reader, mut w: Writer, secs: Duration) -> Result<Bulk, Error> {
    w.write_all(&[BULK]).await?;
    let mut frame = vec![0xa5u8; 4 + CHUNK];
    frame[..4].copy_from_slice(&u32::try_from(CHUNK)?.to_le_bytes());
    let cpu = Sample::now();
    let start = Instant::now();
    let mut bytes = 0u64;
    while start.elapsed() < secs {
        w.write_all(&frame).await?;
        bytes += CHUNK as u64;
        // A QUIC stream write rarely waits, so without this a bulk stream starves a
        // paced task on the same thread.
        tokio::task::yield_now().await;
    }
    w.write_all(&0u32.to_le_bytes()).await?;
    w.flush().await?;
    let mut end = [0u8; 28];
    r.read_exact(&mut end).await?;
    let elapsed = start.elapsed();
    let client = Sample::now().since(cpu);
    let field = |i: usize| {
        let n =
            u64::from_le_bytes(end[4 + 8 * i..12 + 8 * i].try_into().expect("8 bytes"));
        (n != NONE).then_some(n)
    };
    if field(0) != Some(bytes) {
        return Err(format!("sent {bytes} bytes, server counted {:?}", field(0)).into());
    }
    Ok(Bulk {
        bytes,
        elapsed,
        client,
        server: Sample {
            thread: field(1),
            host: field(2),
        },
    })
}

/// A latency frame: length, then sequence and send time in nanoseconds since `base`.
fn frame(size: usize, seq: u64, base: Instant) -> Vec<u8> {
    let mut frame = vec![0u8; 4 + size];
    frame[..4].copy_from_slice(&u32::try_from(size).expect("size fits").to_le_bytes());
    stamp(&mut frame[4..], seq, base);
    frame
}

fn stamp(payload: &mut [u8], seq: u64, base: Instant) {
    payload[..8].copy_from_slice(&seq.to_le_bytes());
    payload[8..16].copy_from_slice(&nanos(base.elapsed()).to_le_bytes());
}

/// Reads sequence and send time from a payload.
fn read_stamp(payload: &[u8]) -> (u64, u64) {
    let word =
        |i: usize| u64::from_le_bytes(payload[i..i + 8].try_into().expect("8 bytes"));
    (word(0), word(8))
}

async fn ping_stream(
    link: &Link,
    size: usize,
    secs: Duration,
) -> Result<Vec<u64>, Error> {
    let (mut r, mut w) = link.open(1).await?;
    w.write_all(&[ECHO]).await?;
    let base = Instant::now();
    let mut back = vec![0u8; 4 + size];
    let mut rtts = Vec::new();
    let mut seq = 0;
    while base.elapsed() < WARMUP + secs {
        let sent = Instant::now();
        w.write_all(&frame(size, seq, base)).await?;
        w.flush().await?;
        r.read_exact(&mut back).await?;
        if sent - base >= WARMUP {
            rtts.push(nanos(sent.elapsed()));
        }
        seq += 1;
    }
    finish(&mut r, &mut w).await?;
    Ok(rtts)
}

async fn ping_datagrams(
    conn: &noq::Connection,
    size: usize,
    secs: Duration,
) -> Result<(Vec<u64>, u64), Error> {
    let base = Instant::now();
    let mut payload = vec![0u8; size];
    let (mut rtts, mut lost) = (Vec::new(), 0);
    let mut seq = 0;
    while base.elapsed() < WARMUP + secs {
        let sent = Instant::now();
        stamp(&mut payload, seq, base);
        conn.send_datagram(payload.clone().into())?;
        let echo = timeout(LOST_AFTER, async {
            loop {
                let back = conn.read_datagram().await?;
                if read_stamp(&back).0 == seq {
                    return Ok::<_, Error>(());
                }
            }
        });
        let measured = sent - base >= WARMUP;
        match echo.await {
            Ok(result) => {
                result?;
                if measured {
                    rtts.push(nanos(sent.elapsed()));
                }
            }
            Err(_) if measured => lost += 1,
            Err(_) => {}
        }
        seq += 1;
    }
    Ok((rtts, lost))
}

async fn paced_stream(
    link: &Link,
    size: usize,
    period: Duration,
    secs: Duration,
) -> Result<Vec<u64>, Error> {
    let (mut r, mut w) = link.open(1).await?;
    w.write_all(&[ECHO]).await?;
    let base = Instant::now();
    let send = async {
        let mut tick = interval(period);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut seq = 0;
        while base.elapsed() < WARMUP + secs {
            tick.tick().await;
            w.write_all(&frame(size, seq, base)).await?;
            w.flush().await?;
            seq += 1;
        }
        w.write_all(&0u32.to_le_bytes()).await?;
        w.flush().await?;
        Ok::<_, Error>(())
    };
    let receive = async {
        let mut back = vec![0u8; 4 + size];
        let mut rtts = Vec::new();
        loop {
            r.read_exact(&mut back[..4]).await?;
            if back[..4] == [0; 4] {
                return Ok::<_, Error>(rtts);
            }
            r.read_exact(&mut back[4..]).await?;
            let (_, sent) = read_stamp(&back[4..]);
            if Duration::from_nanos(sent) >= WARMUP {
                rtts.push(nanos(base.elapsed()) - sent);
            }
        }
    };
    let ((), rtts) = tokio::try_join!(send, receive)?;
    Ok(rtts)
}

async fn paced_datagrams(
    conn: &noq::Connection,
    size: usize,
    period: Duration,
    secs: Duration,
) -> Result<(Vec<u64>, u64), Error> {
    let base = Instant::now();
    let measured = Cell::new(0u64);
    let done = Cell::new(false);
    let send = async {
        let mut tick = interval(period);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut payload = vec![0u8; size];
        let mut seq = 0;
        while base.elapsed() < WARMUP + secs {
            tick.tick().await;
            if base.elapsed() >= WARMUP {
                measured.set(measured.get() + 1);
            }
            stamp(&mut payload, seq, base);
            conn.send_datagram(payload.clone().into())?;
            seq += 1;
        }
        done.set(true);
        Ok::<_, Error>(())
    };
    let receive = async {
        let mut rtts = Vec::new();
        loop {
            match timeout(LOST_AFTER, conn.read_datagram()).await {
                Ok(back) => {
                    let (_, sent) = read_stamp(&back?);
                    if Duration::from_nanos(sent) >= WARMUP {
                        rtts.push(nanos(base.elapsed()) - sent);
                    }
                }
                Err(_) if done.get() => return Ok::<_, Error>(rtts),
                Err(_) => {}
            }
        }
    };
    let ((), rtts) = tokio::try_join!(send, receive)?;
    let lost = measured.get().saturating_sub(rtts.len() as u64);
    Ok((rtts, lost))
}

/// Ends an echo stream and waits for the server's answer.
async fn finish(r: &mut Reader, w: &mut Writer) -> Result<(), Error> {
    w.write_all(&0u32.to_le_bytes()).await?;
    w.flush().await?;
    let mut end = [0u8; 4];
    r.read_exact(&mut end).await?;
    Ok(())
}

fn latency_row(
    link: &Link,
    test: &str,
    size: usize,
    rate: &str,
    mut rtts: Vec<u64>,
    lost: u64,
) -> String {
    rtts.sort_unstable();
    let at = |p: f64| -> String {
        if rtts.is_empty() {
            return "-".into();
        }
        let i = ((p * rtts.len() as f64).ceil() as usize).clamp(1, rtts.len()) - 1;
        format!("{:.1}", rtts[i] as f64 / 1e3)
    };
    format!(
        "| {} | {} | {test} | {size} | {rate} | {} | {lost} | {} | {} | {} | {} |",
        link.carrier,
        gso(link),
        rtts.len(),
        at(0.5),
        at(0.99),
        at(0.999),
        at(1.0),
    )
}

fn gso(link: &Link) -> &'static str {
    if link.gso { "on" } else { "off" }
}

/// CPU nanoseconds per byte.
fn per_byte(ns: Option<u64>, bytes: u64) -> String {
    ns.map_or("-".into(), |ns| format!("{:.3}", ns as f64 / bytes as f64))
}

/// CPU time as a share of one core over `elapsed`.
fn share(ns: Option<u64>, elapsed: Duration) -> String {
    ns.map_or("-".into(), |ns| {
        format!("{:.0}%", ns as f64 / elapsed.as_nanos() as f64 * 100.0)
    })
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).expect("under 584 years")
}
