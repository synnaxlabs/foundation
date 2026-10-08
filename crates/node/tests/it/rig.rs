//! A bench for process tests: one `foundation` node in a temporary directory, and the
//! simulated servers that it reaches on loopback, served by the test process over
//! `os`. Each method with a `todo!` waits on the issue it names.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Output;

use types::quality::Quality;
use types::time::Stamp;

/// One `foundation` node in a temporary directory of its own, with a simulated OPC UA
/// server and a simulated InfluxDB on loopback. Drop removes the directory, and keeps
/// it when the test fails, so its files show what went wrong.
#[derive(Debug)]
pub(crate) struct Rig {
    /// The working directory of each command. It holds `plant.hcl`.
    pub(crate) dir: PathBuf,
    pub(crate) opcua: Opcua,
    pub(crate) influx: Influx,
}

/// The simulated OPC UA server, with no security.
#[derive(Debug, Default)]
pub(crate) struct Opcua {
    /// The address, once it serves.
    address: Option<SocketAddr>,
}

/// The simulated InfluxDB, with the database `plant`.
#[derive(Debug, Default)]
pub(crate) struct Influx {
    /// The address, once it served.
    address: Option<SocketAddr>,
}

/// One sample of an OPC UA variable, as the server served it or as InfluxDB holds it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Sample {
    pub(crate) time: Stamp,
    pub(crate) value: f64,
    /// The OPC UA status. `None` in InfluxDB for a channel with no quality channel.
    pub(crate) quality: Option<Quality>,
}

/// One connector in `foundation status --json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Connector {
    pub(crate) kind: String,
    /// `running`, `restarting`, `stopped`, or `unknown`.
    pub(crate) state: String,
    pub(crate) address: String,
    /// Each count that the kind names, such as `in` and `confirmed`.
    pub(crate) counts: BTreeMap<String, u64>,
    /// The last error, or `None` when the connector has none.
    pub(crate) error: Option<Error>,
}

/// The last error of a connector in `foundation status --json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Error {
    /// `config`, `device`, or `retry`.
    pub(crate) class: String,
    pub(crate) text: String,
}

impl Rig {
    /// Makes a temporary directory for the test on this thread, with a name that no
    /// directory has: a failed run keeps its directory, and a later run can get the
    /// same PID.
    pub(crate) fn new() -> Self {
        let thread = std::thread::current();
        let test = thread.name().expect("invariant: libtest names the thread");
        let name = format!("foundation-node-{}-{test}", std::process::id());
        let name = name.replace("::", "-");
        let mut n = 0;
        let dir = loop {
            let dir = std::env::temp_dir().join(format!("{name}-{n}"));
            match std::fs::create_dir(&dir) {
                Ok(()) => break dir,
                Err(error) if error.kind() == ErrorKind::AlreadyExists => n += 1,
                Err(error) => panic!("make {}: {error}", dir.display()),
            }
        };
        Self {
            dir,
            opcua: Opcua::default(),
            influx: Influx::default(),
        }
    }

    /// Writes `hcl` to `plant.hcl`, with the address of each server that served in
    /// place of `localhost:50000` for OPC UA and `localhost:8086` for InfluxDB.
    pub(crate) fn config(&self, hcl: &str) {
        let mut hcl = hcl.to_owned();
        if let Some(opcua) = self.opcua.address {
            hcl = hcl.replace("localhost:50000", &opcua.to_string());
        }
        if let Some(influx) = self.influx.address {
            hcl = hcl.replace("localhost:8086", &influx.to_string());
        }
        std::fs::write(self.dir.join("plant.hcl"), hcl).expect("write plant.hcl");
    }

    /// Starts `foundation start --name edge`, with each listener of the node on port 0
    /// of loopback, and waits until it prints that the node runs.
    pub(crate) fn start(&mut self) {
        todo!("waits on #1732")
    }

    /// Runs `foundation` with `args`, and gives its output when it exits.
    pub(crate) fn run(&self, _args: &[&str]) -> Output {
        todo!("waits on #1732")
    }

    /// Calls `check` until it gives `Ok`, and gives that value. When 90 s pass first,
    /// longer than the 60 s cap of a restart backoff, panics with `what` and the last
    /// `Err`: the state that `check` saw.
    pub(crate) fn wait<T>(
        &self,
        _what: &str,
        _check: impl FnMut(&Self) -> Result<T, String>,
    ) -> T {
        todo!("waits on #1732")
    }

    /// Plans `plant.hcl` into `plant.plan`, and applies that plan. Panics when either
    /// command does not exit 0.
    pub(crate) fn apply(&self) {
        todo!("waits on #337, #1744")
    }

    /// The connectors of `foundation status --json`, by name.
    pub(crate) fn status(&self) -> BTreeMap<String, Connector> {
        todo!("waits on #1735")
    }
}

impl Opcua {
    /// Serves on a free loopback port: `ns=3;s=SpikeData`, `ns=3;s=DipData`, and
    /// `ns=3;s=PositiveTrendData`, each at a first value until [`Opcua::change`].
    pub(crate) fn serve(&mut self) {
        todo!("waits on #120, #435")
    }

    /// Changes each variable `ticks` times at 10 Hz, each change with the status
    /// `quality`, and returns after the last change.
    pub(crate) fn change(&self, _ticks: u32, _quality: Quality) {
        todo!("waits on #435")
    }

    /// Each sample served, by node id, in order: the first value, then each change.
    pub(crate) fn served(&self) -> BTreeMap<String, Vec<Sample>> {
        todo!("waits on #435")
    }
}

impl Influx {
    /// Serves, and gives the address: a free loopback port at the first call, and the
    /// same port after [`Influx::stop`]. What it stored stays across a stop.
    pub(crate) fn serve(&mut self) -> SocketAddr {
        todo!("waits on #120")
    }

    /// Stops, so nothing listens on the port.
    pub(crate) fn stop(&mut self) {
        todo!("waits on #120")
    }

    /// Each sample stored, by data channel, in time order.
    pub(crate) fn stored(&self) -> BTreeMap<String, Vec<Sample>> {
        todo!("waits on #1734")
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            std::fs::remove_dir_all(&self.dir).expect("remove the directory");
        }
    }
}

#[test]
fn a_rig_takes_a_new_directory_when_its_name_is_in_use() {
    let kept = Rig::new();
    let rig = Rig::new();
    assert_ne!(rig.dir, kept.dir);
}
