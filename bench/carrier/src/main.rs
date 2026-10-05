//! Measures QUIC (noq) against TLS 1.3 over TCP (rustls), both on aws-lc-rs. The
//! client prints one Markdown table line per run. `run.sh` drives the matrix.

#![expect(
    clippy::disallowed_methods,
    reason = "a benchmark reads the real clock and arguments"
)]

mod cpu;
mod link;
mod test;

use std::net::SocketAddr;
use std::time::Duration;

use link::Carrier;
use test::Test;

type Error = Box<dyn std::error::Error + Send + Sync>;

const USAGE: &str = "\
usage:
  carrier server <quic|tls> <listen> <cert.pem> <key.pem> [no-gso]
  carrier client <quic|quic-dgram|tls> <server> <ca.pem> [no-gso] <test>
tests:
  bulk <secs>                        one stream, as fast as it goes
  ping <size> <secs>                 one frame in flight, echoed
  paced <size> <rate> <secs> [load]  frames at a fixed rate, echoed; `load` adds a
                                     bulk stream on the same connection and thread
                                     (QUIC) or thread (TLS)";

/// The server name in the benchmark certificate.
const SERVER_NAME: &str = "carrier.test";

fn main() -> Result<(), Error> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| "a crypto provider is already installed")?;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["server", carrier, listen, cert, key, rest @ ..] => {
            let gso = !rest.contains(&"no-gso");
            let carrier = carrier.parse()?;
            runtime.block_on(link::serve(carrier, listen.parse()?, cert, key, gso))
        }
        ["client", carrier, server, ca, rest @ ..] => {
            let (gso, rest) = match rest {
                ["no-gso", rest @ ..] => (false, rest),
                rest => (true, rest),
            };
            let carrier: Carrier = carrier.parse()?;
            let test = parse_test(rest)?;
            let server: SocketAddr = server.parse()?;
            runtime.block_on(async {
                let link = link::Link::connect(carrier, server, ca, gso).await?;
                let row = test::run(&link, &test).await?;
                println!("{row}");
                Ok(())
            })
        }
        _ => Err(USAGE.into()),
    }
}

fn parse_test(args: &[&str]) -> Result<Test, Error> {
    let secs =
        |s: &str| -> Result<Duration, Error> { Ok(Duration::from_secs(s.parse()?)) };
    match args {
        ["bulk", s] => Ok(Test::Bulk { secs: secs(s)? }),
        ["ping", size, s] => Ok(Test::Ping {
            size: size.parse()?,
            secs: secs(s)?,
        }),
        ["paced", size, rate, s, rest @ ..] => Ok(Test::Paced {
            size: size.parse()?,
            rate: rate.parse()?,
            secs: secs(s)?,
            load: rest == ["load"],
        }),
        _ => Err(format!("unknown test {args:?}").into()),
    }
}
