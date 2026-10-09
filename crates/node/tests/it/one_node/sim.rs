//! The simulated servers of ONE NODE, served by the test process over `os` on
//! loopback. Each method with a `todo!` waits on the issue it names.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroUsize;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use connector::cancel::Token;
use connector::http::Client;
use connector_influx::sim::Store;
use env::net::tcp::{Listen, Options};
use env::thread::Handle;
use http::Request;
use types::quality::Quality;
use types::time::{Span, Stamp};

/// The options of each stream that a simulated server accepts.
const OPTIONS: Options = Options {
    send_buffer_bytes: 1 << 16,
    recv_buffer_bytes: 1 << 16,
    unsent_bytes_max: NonZeroUsize::new(1 << 14).unwrap(),
    delayed: false,
};

/// The simulated OPC UA server, with no security.
#[derive(Debug, Default)]
pub(super) struct Opcua {}

/// The simulated InfluxDB, with the database `plant`. Drop stops it.
#[derive(Debug, Default)]
pub(super) struct Influx {
    /// The tests read it while [`Influx::stored`] waits on #1734. The test
    /// `a_dropped_influx_ends_its_shard` counts its owners.
    store: Arc<Mutex<Store>>,
    /// The address of the first [`Influx::serve`].
    address: Option<SocketAddr>,
    /// The token that stops the shard that serves, and that shard. The panic tests put
    /// a shard that panics or blocks here, since no input to [`Influx::serve`] makes
    /// it do so, and read the token that a drop cancels.
    serving: Option<(Token, Handle)>,
}

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
        todo!("waits on #435")
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
        let local = self.address.unwrap_or((Ipv4Addr::LOCALHOST, 0).into());
        let listen = Listen {
            local,
            backlog: 128,
            options: OPTIONS,
        };
        let listener = (os::net().listen(&listen))
            .unwrap_or_else(|error| panic!("listen on {local}: {error}"));
        let address = listener.local();
        self.address = Some(address);
        let (stop, store) = (Token::new(), Arc::clone(&self.store));
        let stopped = stop.clone();
        let shard = start("influx", move |tasks| async move {
            let serve =
                connector_influx::sim::serve(listener, tasks, store, "plant".into());
            if let Some(error) = stopped.race(serve).await {
                panic!("the simulated InfluxDB stopped: {error}");
            }
        });
        self.serving = Some((stop, shard));
        address
    }

    /// Stops, and panics when the server panicked. A process that holds or takes the
    /// port before the next [`Influx::serve`] makes that serve panic.
    pub(super) fn stop(&mut self) {
        let shard = self.cancel().expect("it serves");
        shard
            .join()
            .expect("the simulated InfluxDB stops with no panic");
    }

    /// Cancels the server, and gives its shard to join, or `None` when it does not
    /// serve.
    fn cancel(&mut self) -> Option<Handle> {
        let (stop, shard) = self.serving.take()?;
        stop.cancel();
        Some(shard)
    }

    /// Each sample stored, by data channel, in time order.
    pub(super) fn stored(&self) -> BTreeMap<String, Vec<Sample>> {
        todo!("waits on #1734")
    }
}

impl Drop for Influx {
    fn drop(&mut self) {
        // While the thread panics, a join can block and a second panic aborts the
        // test binary. The server stops on its own.
        if let Some(shard) = self.cancel()
            && !std::thread::panicking()
        {
            shard
                .join()
                .expect("the simulated InfluxDB serves with no panic");
        }
    }
}

/// Starts `main` on a new `os` shard named `name`.
fn start<F>(
    name: &str,
    main: impl FnOnce(env::tasks::Tasks) -> F + Send + 'static,
) -> Handle
where
    F: Future<Output = ()> + 'static,
{
    let config = env::shards::Config {
        name: name.into(),
        core: None,
    };
    let shards = os::shards().expect("read the cores");
    shards.start(config, main).expect("start the shard")
}

/// Writes `lines` to the database `plant` at `address` from a shard of its own, and
/// gives the status of the answer or the error of the client.
fn write(address: SocketAddr, lines: &'static str) -> String {
    let (send, answer) = mpsc::channel();
    let shard = start("write", move |tasks| async move {
        let client = Client::new(connector::http::Config {
            net: os::net(),
            clock: os::clock(),
            tasks,
            timeout: Span::MINUTE,
            body_max: 1 << 10,
        });
        let request = Request::post(format!("http://{address}/write?db=plant"))
            .body(Bytes::from_static(lines.as_bytes()))
            .expect("a valid request");
        let answer = match client.send(request).await {
            Ok(answer) => answer.status().to_string(),
            Err(error) => error.to_string(),
        };
        send.send(answer).expect("the test reads the answer");
    });
    shard.join().expect("the write runs with no panic");
    answer.try_recv().expect("the shard sent the answer")
}

/// What [`write`] gives when no server listens at `address`. On macOS, a child that a
/// rig test spawns may hold the port (`os::net`), so only Linux checks a refusal.
fn refused(address: SocketAddr) -> String {
    format!("the connect failed: {address} refused the connection")
}

#[test]
fn influx_stores_each_line_written_to_plant_and_keeps_it_after_a_stop() {
    let mut influx = Influx::default();
    let address = influx.serve();
    assert_eq!(write(address, "m f=1 1\nm f=2 2"), "204 No Content");
    influx.stop();
    let store = influx.store.lock().expect("no panic");
    let times: Vec<_> = store.points("m", &[]).map(|point| point.time).collect();
    assert_eq!(times, [Stamp::from_nanos(1), Stamp::from_nanos(2)]);
}

#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "needs SOCK_CLOEXEC and the shutdown of a listener"
)]
fn a_stopped_influx_refuses_and_serves_again_on_its_port_with_what_it_stored() {
    let mut influx = Influx::default();
    let address = influx.serve();
    assert_eq!(write(address, "m f=1 1"), "204 No Content");
    influx.stop();
    assert_eq!(write(address, "m f=2 2"), refused(address));
    assert_eq!(influx.serve(), address);
    assert_eq!(write(address, "m f=3 3"), "204 No Content");
    let store = influx.store.lock().expect("no panic");
    let times: Vec<_> = store.points("m", &[]).map(|point| point.time).collect();
    assert_eq!(times, [Stamp::from_nanos(1), Stamp::from_nanos(3)]);
}

#[test]
fn a_dropped_influx_ends_its_shard() {
    let mut influx = Influx::default();
    let address = influx.serve();
    assert_eq!(write(address, "m f=1 1"), "204 No Content");
    let store = Arc::clone(&influx.store);
    drop(influx);
    assert_eq!(Arc::strong_count(&store), 1, "the shard holds no store");
    if cfg!(target_os = "linux") {
        assert_eq!(write(address, "m f=2 2"), refused(address));
    }
}

#[test]
fn a_dropped_influx_does_not_join_its_server_when_the_test_panics() {
    let (release, released) = mpsc::channel::<()>();
    let mut influx = Influx::default();
    let shard = start("influx", move |_| async move {
        #[expect(
            clippy::disallowed_methods,
            reason = "the shard blocks until the test releases it"
        )]
        let _waited = released.recv_timeout(std::time::Duration::from_secs(30));
    });
    influx.serving = Some((Token::new(), shard));
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _influx = influx;
        panic!("the test broke");
    }));
    unwound.expect_err("the test panics");
    release
        .send(())
        .expect("the drop did not wait for the server");
}

#[test]
fn a_dropped_influx_cancels_its_server_when_the_test_panics() {
    let mut influx = Influx::default();
    influx.serve();
    let stop = influx.serving.as_ref().expect("it serves").0.clone();
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _influx = influx;
        panic!("the test broke");
    }));
    unwound.expect_err("the test panics");
    assert!(stop.cancelled(), "the server serves on");
}

#[test]
#[should_panic(expected = "the simulated InfluxDB serves with no panic")]
fn a_dropped_influx_panics_when_its_server_panicked() {
    let mut influx = Influx::default();
    let shard = start("influx", |_| async { panic!("the server broke") });
    influx.serving = Some((Token::new(), shard));
    drop(influx);
}

#[test]
#[should_panic(expected = "the simulated InfluxDB stops with no panic")]
fn a_stopped_influx_panics_when_its_server_panicked() {
    let mut influx = Influx::default();
    let shard = start("influx", |_| async { panic!("the server broke") });
    influx.serving = Some((Token::new(), shard));
    influx.stop();
}

#[test]
#[should_panic(expected = "the test broke")]
fn a_dropped_influx_does_not_panic_again_when_the_test_panics() {
    let mut influx = Influx::default();
    let shard = start("influx", |_| async { panic!("the server broke") });
    influx.serving = Some((Token::new(), shard));
    panic!("the test broke");
}
