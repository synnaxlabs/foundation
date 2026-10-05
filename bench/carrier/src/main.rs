//! Measures QUIC (noq) against TLS 1.3 over TCP (tokio-rustls) between two hosts.
//! The client prints one Markdown table line per run. `run.sh` drives the matrix.

#![expect(
    clippy::disallowed_methods,
    reason = "a benchmark reads the real clock, its arguments, and /proc"
)]
#![expect(clippy::print_stdout, reason = "the client prints its table line")]
#![expect(
    clippy::print_stderr,
    reason = "the server reports errors, and the client QUIC's counters"
)]

mod cpu;
mod session;
mod test;

use std::net::SocketAddr;
use std::sync::Arc;

use session::{Config, Listener, Session};
use test::Test;

type Error = Box<dyn std::error::Error + Send + Sync>;

const USAGE: &str = "\
usage:
  carrier columns <bulk|latency>    the header of the client's table lines
  carrier server <quic|tls> <listen> <cert.pem> <key.pem> [options]
  carrier client <quic|tls> <server> <ca.pem> <test> [options]
tests:
  bulk <secs>                       one stream, as fast as it goes
  ping <frames> <size> <secs>       one frame in flight, echoed
  paced <frames> <size> <rate> <secs> <none|shared>
                                    frames at a fixed rate, echoed; `shared` adds a
                                    bulk flow on the same connection and thread
  <frames> is `stream` or `datagram` (QUIC only).
options:
  unsegmented      QUIC sends without GSO
  mtu=<bytes>      the link MTU (1500)
  cpus=<a,b,..>    count the busy time of these CPUs
  samples=<path>   write each round trip there (client)";

fn main() -> Result<(), Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let provider = Arc::new(Config::provider());
    match args.as_slice() {
        ["columns", "bulk"] => println!("{}", test::BULK_COLUMNS),
        ["columns", "latency"] => println!("{}", test::LATENCY_COLUMNS),
        ["server", carrier, listen, cert, key, rest @ ..] => {
            let options = Options::parse(rest, Role::Server)?;
            let config = Config::new(
                carrier.parse()?,
                provider,
                options.unsegmented,
                options.mtu,
            )?;
            runtime.block_on(async {
                let listener =
                    Listener::bind(&config, listen.parse()?, cert, key).await?;
                eprintln!("server on {}: {config}", listener.local_addr()?);
                serve(listener, options.cpus.into()).await
            })?;
        }
        ["client", carrier, server, ca, rest @ ..] => {
            let (test, rest) = Test::parse(rest)?;
            let options = Options::parse(rest, Role::Client)?;
            let config = Config::new(
                carrier.parse()?,
                provider,
                options.unsegmented,
                options.mtu,
            )?;
            runtime.block_on(client(&config, server.parse()?, ca, &test, &options))?;
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

/// Which end of a session the options are for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Server,
    Client,
}

/// The arguments after the positional ones.
struct Options {
    unsegmented: bool,
    mtu: u16,
    cpus: Vec<usize>,
    samples: Option<String>,
}

impl Options {
    fn parse(args: &[&str], role: Role) -> Result<Self, Error> {
        let mut options = Self {
            unsegmented: false,
            mtu: 1500,
            cpus: Vec::new(),
            samples: None,
        };
        for arg in args {
            match arg.split_once('=') {
                None if *arg == "unsegmented" => options.unsegmented = true,
                Some(("mtu", n)) => options.mtu = n.parse()?,
                Some(("cpus", list)) => {
                    options.cpus =
                        list.split(',').map(str::parse).collect::<Result<_, _>>()?;
                }
                Some(("samples", path)) if role == Role::Client => {
                    options.samples = Some(path.into());
                }
                _ => return Err(format!("unknown option {arg:?}\n{USAGE}").into()),
            }
        }
        Ok(options)
    }
}

/// Serves each session the listener accepts on its own task.
async fn serve(listener: Listener, cpus: Arc<[usize]>) -> Result<(), Error> {
    loop {
        let handshake = listener.accept().await?;
        let cpus = Arc::clone(&cpus);
        tokio::spawn(report("session", async move {
            test::serve(handshake.finish().await?, cpus).await
        }));
    }
}

async fn client(
    config: &Config,
    server: SocketAddr,
    ca: &str,
    test: &Test,
    options: &Options,
) -> Result<(), Error> {
    let mut session = Session::connect(config, server, ca).await?;
    let outcome = test::run(&mut session, test, &options.cpus).await?;
    if let Some(stats) = session.stats() {
        eprintln!("noq: {stats}");
    }
    session.close().await;
    println!("{}", outcome.line());
    if let Some(path) = &options.samples {
        std::fs::write(path, outcome.samples())?;
    }
    Ok(())
}

/// Runs a server task and prints its error, which `run.sh` counts as a failure.
async fn report(what: &'static str, task: impl Future<Output = Result<(), Error>>) {
    if let Err(e) = task.await {
        eprintln!("error: {what}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_reject_an_unknown_option() {
        let error = Options::parse(&["nogso"], Role::Server)
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.starts_with("unknown option \"nogso\"\nusage:"),
            "{error}"
        );
    }

    #[test]
    fn options_reject_samples_on_the_server() {
        let error = Options::parse(&["samples=x"], Role::Server)
            .err()
            .unwrap()
            .to_string();
        assert!(error.starts_with("unknown option \"samples=x\""), "{error}");
    }

    #[test]
    fn options_parse_each_option() {
        let args = ["unsegmented", "mtu=9001", "cpus=2,6", "samples=out"];
        let options = Options::parse(&args, Role::Client).unwrap();
        assert!(options.unsegmented);
        assert_eq!(options.mtu, 9001);
        assert_eq!(options.cpus, [2, 6]);
        assert_eq!(options.samples.as_deref(), Some("out"));
    }

    #[test]
    fn config_rejects_an_mtu_below_the_quic_minimum() {
        let provider = Arc::new(Config::provider());
        let error = Config::new(session::Carrier::Quic, provider, false, 1227)
            .err()
            .unwrap();
        assert_eq!(
            error.to_string(),
            "MTU 1227 is below the QUIC minimum of 1228"
        );
    }
}
