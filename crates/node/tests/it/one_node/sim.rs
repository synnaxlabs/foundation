//! The simulated servers of ONE NODE, served by the test process over `os` on
//! loopback. Each method with a `todo!` waits on the issue it names.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use types::quality::Quality;
use types::time::Stamp;

/// The simulated OPC UA server, with no security.
#[derive(Debug, Default)]
pub(super) struct Opcua {}

/// The simulated InfluxDB, with the database `plant`.
#[derive(Debug, Default)]
pub(super) struct Influx {}

/// One sample of an OPC UA variable, as the server served it or as InfluxDB holds it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Sample {
    pub(super) time: Stamp,
    pub(super) value: f64,
    /// The OPC UA status. `None` in InfluxDB for a channel with no quality channel.
    pub(super) quality: Option<Quality>,
}

impl Opcua {
    /// Serves on a free loopback port, and gives the address: `ns=3;s=SpikeData`,
    /// `ns=3;s=DipData`, and `ns=3;s=PositiveTrendData`, each at a first value until
    /// [`Opcua::change`].
    pub(super) fn serve(&mut self) -> SocketAddr {
        todo!("waits on #120, #435")
    }

    /// Changes each variable `ticks` times at 10 Hz, each change with the status
    /// `quality`, and returns after the last change.
    pub(super) fn change(&self, _ticks: usize, _quality: Quality) {
        todo!("waits on #435")
    }

    /// Each sample served, by node id, in order: the first value, then each change.
    pub(super) fn served(&self) -> BTreeMap<String, Vec<Sample>> {
        todo!("waits on #435")
    }
}

impl Influx {
    /// Serves, and gives the address: a free loopback port at the first call, and the
    /// same port after [`Influx::stop`]. What it stored stays across a stop.
    pub(super) fn serve(&mut self) -> SocketAddr {
        todo!("waits on #120")
    }

    /// Stops, so nothing listens on the port. Another process can take the port
    /// before the next [`Influx::serve`], which then panics.
    pub(super) fn stop(&mut self) {
        todo!("waits on #120")
    }

    /// Each sample stored, by data channel, in time order.
    pub(super) fn stored(&self) -> BTreeMap<String, Vec<Sample>> {
        todo!("waits on #1734")
    }
}
