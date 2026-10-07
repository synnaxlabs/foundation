use std::net::{IpAddr, SocketAddr};

use bytes::Bytes;
use env::net;
use sim::name::{self, Answer};
use sim::node;
use types::time::Span;

use super::pool::{all_ok, ok};
use super::{Network, PORT, Step, TIMEOUT, get, text};
use crate::http::Error;

impl Network {
    /// Makes `host` name `addresses` for the client.
    fn name(&mut self, host: &str, addresses: Vec<IpAddr>, delay: Span) {
        let answer = Answer::Addresses(addresses);
        self.sim.name(host, name::Config { answer, delay });
    }

    /// The address of a new node that listens on no port.
    fn deaf(&mut self) -> IpAddr {
        self.sim.node(node::Config::default()).addresses()[0]
    }
}

#[test]
fn sends_to_the_address_of_a_name() {
    let mut network = Network::new(70);
    let server = network.remote().ip();
    network.name("influx", vec![server], Span::ZERO);
    let seen = network.serve(|mut stream, _, _| async move {
        super::write(&mut stream, b"HTTP/1.1 204 No Content\r\n\r\n").await;
        Some(stream)
    });
    let response = network
        .send(get(&format!("http://influx:{PORT}/ping")))
        .expect("the server answers");
    assert_eq!(response.status(), 204);
    let seen = text(&seen.lock().expect("no panic"));
    assert!(seen.starts_with("GET /ping HTTP/1.1\r\n"), "{seen}");
    assert!(seen.contains(&format!("host: influx:{PORT}\r\n")), "{seen}");
}

#[test]
fn gives_the_lookup_error_of_an_unknown_name() {
    let mut network = Network::new(71);
    let error = network
        .send(get("http://nowhere/"))
        .expect_err("no address");
    assert!(
        matches!(
            &error,
            Error::Connect(net::Error::NotFound { host }) if host == "nowhere"
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "the connect failed: name nowhere has no address"
    );
}

#[test]
fn gives_the_lookup_error_when_no_name_server_answers() {
    let mut network = Network::new(72);
    let config = name::Config {
        answer: Answer::Failed,
        ..name::Config::default()
    };
    network.sim.name("influx", config);
    let error = network
        .send(get("http://influx/"))
        .expect_err("a failed lookup");
    assert!(
        matches!(error, Error::Connect(net::Error::Io { code: 11 })),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        "the connect failed: network call failed with OS error 11"
    );
}

#[test]
fn connects_to_the_next_address_when_one_refuses() {
    let mut network = Network::new(73);
    let (deaf, server) = (network.deaf(), network.remote().ip());
    network.name("influx", vec![deaf, server], Span::ZERO);
    let log = network.serve_each(PORT, ok);
    let outcomes =
        network.run(vec![Step::Send(get(&format!("http://influx:{PORT}/")))]);
    all_ok(&outcomes);
    assert_eq!(log.lock().expect("no panic").requests, [0]);
}

#[test]
fn gives_the_error_of_the_first_address_when_all_refuse() {
    let mut network = Network::new(74);
    let (first, second) = (network.deaf(), network.deaf());
    network.name("influx", vec![first, second], Span::ZERO);
    let error = network
        .send(get(&format!("http://influx:{PORT}/")))
        .expect_err("no listener");
    let remote = SocketAddr::new(first, PORT);
    assert!(
        matches!(
            error,
            Error::Connect(net::Error::Refused { remote: r }) if r == remote
        ),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!("the connect failed: {remote} refused the connection")
    );
}

#[test]
fn a_reused_stream_does_no_lookup() {
    let mut network = Network::new(75);
    let server = network.remote().ip();
    network.name("influx", vec![server], Span::SECOND);
    let log = network.serve_each(PORT, ok);
    let url = format!("http://influx:{PORT}/");
    let outcomes = network.run(vec![Step::Send(get(&url)), Step::Send(get(&url))]);
    all_ok(&outcomes);
    let elapsed = network.elapsed.expect("a send ran");
    assert!(elapsed < Span::SECOND, "{elapsed}");
    assert_eq!(log.lock().expect("no panic").streams.len(), 1);
}

#[test]
fn names_in_any_case_share_one_stream() {
    let mut network = Network::new(76);
    let server = network.remote().ip();
    network.name("influx", vec![server], Span::ZERO);
    let log = network.serve_each(PORT, ok);
    let outcomes = network.run(vec![
        Step::Send(get(&format!("http://influx:{PORT}/"))),
        Step::Send(get(&format!("http://INFLUX:{PORT}/"))),
    ]);
    all_ok(&outcomes);
    assert_eq!(log.lock().expect("no panic").requests, [0, 0]);
}

#[test]
fn times_out_when_the_lookup_is_slower_than_the_timeout() {
    let mut network = Network::new(77);
    let server = network.remote().ip();
    let delay = Span::from_nanos(TIMEOUT.nanos() + Span::SECOND.nanos());
    network.name("influx", vec![server], delay);
    let error = network
        .send(get(&format!("http://influx:{PORT}/")))
        .expect_err("a slow lookup");
    assert!(matches!(error, Error::TimedOut), "{error:?}");
    assert_eq!(network.elapsed, Some(TIMEOUT));
}

#[test]
fn sends_a_body_to_a_name() {
    let mut network = Network::new(78);
    let server = network.remote().ip();
    network.name("influx", vec![server], Span::ZERO);
    let log = network.serve_each(PORT, ok);
    let request = http::Request::post(format!("http://influx:{PORT}/write"))
        .body(Bytes::from_static(b"m v=1 5"))
        .expect("a valid request");
    all_ok(&network.run(vec![Step::Send(request)]));
    let sent = text(&log.lock().expect("no panic").bytes[0]);
    assert!(sent.ends_with("\r\n\r\nm v=1 5"), "{sent}");
}
