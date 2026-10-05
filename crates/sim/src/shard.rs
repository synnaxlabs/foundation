//! Faults for the shard starts of a node.

use env::shards::Config;

/// A fault that the next shard start on a core gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// `start` gives `thread::Error::Start` with reason `injected`.
    Start,
    /// `start` gives `thread::Error::Pin` with the core.
    Pin,
    /// The shard panics with message `injected`: in a task that the scheduler runs
    /// before or after the first poll of the main future, or as the main future
    /// completes, when that comes first.
    Panic,
}

/// The shard starts of one node, and the faults aimed at them.
#[derive(Default)]
pub(crate) struct Starts {
    /// The config of each start, in order.
    configs: Vec<Config>,
    /// Each fault fails the next start on its core.
    faults: Vec<(usize, Fault)>,
}

impl Starts {
    pub(crate) fn fail(&mut self, core: usize, fault: Fault) {
        self.faults.push((core, fault));
    }

    /// Records a start with `config`, and takes the first fault aimed at its core,
    /// with the core.
    pub(crate) fn record(&mut self, config: &Config) -> Option<(usize, Fault)> {
        self.configs.push(config.clone());
        let core = config.core?;
        let at = self.faults.iter().position(|&(aimed, _)| aimed == core)?;
        Some(self.faults.remove(at))
    }

    pub(crate) fn configs(&self) -> Vec<Config> {
        self.configs.clone()
    }
}
