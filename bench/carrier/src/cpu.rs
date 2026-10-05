//! CPU time from Linux `/proc`. Other systems report none.

use crate::Error;

/// CPU time used so far, in nanoseconds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Sample {
    /// This thread, user and kernel. Softirq work that runs on its CPU is not in it.
    pub(crate) thread: Option<u64>,
    /// Busy time of the listed CPUs: user, system, IRQ, and softirq. `run.sh` lists
    /// the CPU of the process and the CPU that takes the NIC interrupts.
    pub(crate) cpus: Option<u64>,
}

impl Sample {
    #[cfg(target_os = "linux")]
    pub(crate) fn now(cpus: &[usize]) -> Result<Self, Error> {
        Ok(Self {
            thread: Some(linux::thread_ns()?),
            cpus: if cpus.is_empty() {
                None
            } else {
                Some(linux::cpus_ns(cpus)?)
            },
        })
    }

    #[cfg(not(target_os = "linux"))]
    #[expect(clippy::unnecessary_wraps, reason = "the Linux version can fail")]
    pub(crate) fn now(_: &[usize]) -> Result<Self, Error> {
        Ok(Self {
            thread: None,
            cpus: None,
        })
    }

    /// CPU time from `start` to this sample.
    pub(crate) fn since(self, start: Self) -> Self {
        let delta = |end: Option<u64>, start: Option<u64>| {
            end.zip(start).map(|(end, start)| {
                end.checked_sub(start)
                    .expect("invariant: CPU time never goes back")
            })
        };
        Self {
            thread: delta(self.thread, start.thread),
            cpus: delta(self.cpus, start.cpus),
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use crate::Error;

    /// `USER_HZ`: `/proc/stat` counts in ticks of 10 ms on mainstream Linux.
    const TICK_NS: u64 = 10_000_000;

    /// The first field of `/proc/thread-self/schedstat`: time on a CPU.
    pub(super) fn thread_ns() -> Result<u64, Error> {
        let text = std::fs::read_to_string("/proc/thread-self/schedstat")?;
        let ns = text.split_whitespace().next().ok_or("empty schedstat")?;
        Ok(ns.parse()?)
    }

    pub(super) fn cpus_ns(cpus: &[usize]) -> Result<u64, Error> {
        let text = std::fs::read_to_string("/proc/stat")?;
        let mut ticks = 0;
        for cpu in cpus {
            let name = format!("cpu{cpu}");
            let fields: Vec<u64> = text
                .lines()
                .find_map(|line| line.strip_prefix(&name)?.strip_prefix(' '))
                .ok_or_else(|| format!("no {name} in /proc/stat"))?
                .split_whitespace()
                .map(str::parse)
                .collect::<Result<_, _>>()?;
            let [user, nice, system, _idle, _iowait, irq, softirq, ..] = fields[..]
            else {
                return Err(format!("short {name} line in /proc/stat").into());
            };
            ticks += user + nice + system + irq + softirq;
        }
        Ok(ticks * TICK_NS)
    }
}
