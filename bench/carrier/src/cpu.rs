//! CPU time, read from Linux `/proc`. Other systems report none.

/// Clock ticks per second in `/proc/stat`: `USER_HZ`, 100 on mainstream Linux.
const TICK_NS: u64 = 10_000_000;

/// CPU time used so far, in nanoseconds.
#[derive(Clone, Copy)]
pub struct Sample {
    /// This thread: user and kernel time, including the network stack work it runs.
    pub thread: Option<u64>,
    /// All cores of the host, busy time: user, system, IRQ, and softirq.
    pub host: Option<u64>,
}

impl Sample {
    pub fn now() -> Self {
        Self {
            thread: thread_ns(),
            host: host_ns(),
        }
    }

    /// CPU time from `start` to this sample.
    pub fn since(self, start: Self) -> Self {
        Self {
            thread: self.thread.zip(start.thread).map(|(a, b)| a - b),
            host: self.host.zip(start.host).map(|(a, b)| a - b),
        }
    }
}

/// The first field of `/proc/thread-self/schedstat`: time on a CPU in nanoseconds.
fn thread_ns() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/thread-self/schedstat").ok()?;
    text.split_whitespace().next()?.parse().ok()
}

/// Busy ticks on all cores from the first line of `/proc/stat`.
fn host_ns() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/stat").ok()?;
    let fields: Vec<u64> = text
        .lines()
        .next()?
        .split_whitespace()
        .skip(1)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    // user nice system idle iowait irq softirq steal ...
    let busy = fields[0] + fields[1] + fields[2] + fields[5] + fields[6];
    Some(busy * TICK_NS)
}
