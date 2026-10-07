use bytes::Bytes;
use http::Request;

use types::time::{Monotonic, Span};

use super::common::{OK, Reply, all_ok, ok};
use super::{BODY_MAX, Network, PORT, Step, get};
use crate::http::Error;

const IDLE_MAX: Span = Span::from_nanos(90 * Span::SECOND.nanos());
const OVER: Span = Span::from_nanos(IDLE_MAX.nanos() + 1);

fn after(time: Monotonic, span: Span) -> Monotonic {
    time.checked_add(span).expect("a short run")
}

fn sends(network: &Network, port: u16, n: usize) -> Vec<Step> {
    let url = format!("http://{}/", network.remote_on(port));
    (0..n).map(|_| Step::Send(get(&url))).collect()
}

#[test]
fn two_sends_to_one_origin_use_one_stream() {
    let mut network = Network::new(20);
    let log = network.serve_each(PORT, ok);
    let outcomes = network.run(sends(&network, PORT, 2));
    all_ok(&outcomes);
    assert_eq!(log.lock().expect("no panic").requests, [0, 0]);
}

#[test]
fn reuses_a_stream_idle_for_exactly_90_s() {
    let mut network = Network::new(21);
    let log = network.serve_each(PORT, ok);
    let mut steps = sends(&network, PORT, 2);
    steps.insert(1, Step::Wait(IDLE_MAX));
    all_ok(&network.run(steps));
    assert_eq!(log.lock().expect("no panic").requests, [0, 0]);
}

#[test]
fn opens_a_new_stream_after_90_s_idle() {
    let mut network = Network::new(22);
    let log = network.serve_each(PORT, ok);
    let mut steps = sends(&network, PORT, 2);
    steps.insert(1, Step::Wait(OVER));
    all_ok(&network.run(steps));
    let log = log.lock().expect("no panic");
    assert_eq!(log.requests, [0, 1]);
    let first = log.streams[0].1.expect("the old stream ended");
    let second = log.streams[1].1.expect("the drop ended the new stream");
    assert!(first < second, "{first:?} {second:?}");
}

#[test]
fn opens_a_new_stream_when_the_server_closed_the_old_one() {
    let mut network = Network::new(23);
    let log = network.serve_each(PORT, |_| Reply::Close(OK.into(), Span::ZERO));
    let mut steps = sends(&network, PORT, 2);
    steps.insert(1, Step::Wait(Span::SECOND));
    all_ok(&network.run(steps));
    assert_eq!(log.lock().expect("no panic").requests, [0, 1]);
}

#[test]
fn opens_a_new_stream_after_connection_close() {
    let mut network = Network::new(24);
    let log = network.serve_each(PORT, |_| {
        Reply::Bytes(
            "HTTP/1.1 200 OK\r\nconnection: close\r\ncontent-length: 2\r\n\r\nok"
                .into(),
        )
    });
    all_ok(&network.run(sends(&network, PORT, 2)));
    assert_eq!(log.lock().expect("no panic").requests, [0, 1]);
}

#[test]
fn opens_a_new_stream_after_a_timeout() {
    let mut network = Network::new(25);
    let log = network.serve_each(PORT, |n| if n == 0 { Reply::Nothing } else { ok(n) });
    let outcomes = network.run(sends(&network, PORT, 2));
    assert!(
        matches!(outcomes[0], Err(Error::TimedOut)),
        "{:?}",
        outcomes[0]
    );
    all_ok(&outcomes[1..]);
    assert_eq!(log.lock().expect("no panic").requests, [0, 1]);
}

#[test]
fn opens_a_new_stream_after_a_body_over_the_cap() {
    let mut network = Network::new(26);
    let large = format!(
        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}",
        BODY_MAX + 1,
        "x".repeat(BODY_MAX + 1)
    );
    let log = network.serve_each(PORT, move |n| {
        if n == 0 {
            Reply::Bytes(large.clone())
        } else {
            ok(n)
        }
    });
    let outcomes = network.run(sends(&network, PORT, 2));
    assert!(
        matches!(outcomes[0], Err(Error::TooLarge { max: BODY_MAX })),
        "{:?}",
        outcomes[0]
    );
    all_ok(&outcomes[1..]);
    assert_eq!(log.lock().expect("no panic").requests, [0, 1]);
}

#[test]
fn two_origins_use_two_streams() {
    let mut network = Network::new(27);
    let a = network.serve_each(PORT, ok);
    let b = network.serve_each(PORT + 1, ok);
    let mut steps = Vec::new();
    for _ in 0..2 {
        steps.extend(sends(&network, PORT, 1));
        steps.extend(sends(&network, PORT + 1, 1));
    }
    all_ok(&network.run(steps));
    assert_eq!(a.lock().expect("no panic").requests, [0, 0]);
    assert_eq!(b.lock().expect("no panic").requests, [0, 0]);
}

#[test]
fn a_send_drops_an_idle_stream_to_another_origin_after_90_s() {
    let mut network = Network::new(28);
    let a = network.serve_each(PORT, ok);
    let b = network.serve_each(PORT + 1, ok);
    let mut steps = sends(&network, PORT, 1);
    steps.push(Step::Wait(OVER));
    steps.extend(sends(&network, PORT + 1, 1));
    steps.push(Step::Wait(Span::MINUTE));
    all_ok(&network.run(steps));
    let a = a.lock().expect("no panic").streams[0]
        .1
        .expect("the idle stream ended");
    let b = b.lock().expect("no panic").streams[0]
        .1
        .expect("the drop ended it");
    assert!(after(a, Span::MINUTE) <= b, "{a:?} {b:?}");
}

#[test]
fn keeps_the_stream_open_until_the_client_drops() {
    let mut network = Network::new(29);
    let log = network.serve_each(PORT, ok);
    let mut steps = sends(&network, PORT, 1);
    steps.push(Step::Wait(Span::MINUTE));
    all_ok(&network.run(steps));
    let (start, end) = log.lock().expect("no panic").streams[0];
    let end = end.expect("the drop ended it");
    assert!(after(start, Span::MINUTE) <= end, "{start:?} {end:?}");
}

/// The steps of two sends, where the second goes out after the server closed the
/// idle stream, but before its FIN reaches the client.
fn race(network: &mut Network, second: Request<Bytes>) -> Vec<Step> {
    race_at(
        network,
        second,
        RACE_IDLE.nanos() - 100 * Span::MICROSECOND.nanos(),
    )
}

/// The steps of two sends, `wait` nanoseconds apart.
fn race_at(network: &mut Network, second: Request<Bytes>, wait: i64) -> Vec<Step> {
    let mut steps = sends(network, PORT, 1);
    steps.push(Step::Wait(Span::from_nanos(wait)));
    steps.push(Step::Send(second));
    steps
}

const RACE_IDLE: Span = Span::from_nanos(10 * Span::SECOND.nanos());

#[test]
fn sends_a_get_again_when_the_server_closed_the_idle_stream_first() {
    let mut network = Network::new(31);
    let log = network.serve_each(PORT, |_| Reply::Close(OK.into(), RACE_IDLE));
    let url = format!("http://{}/", network.remote());
    let steps = race(&mut network, get(&url));
    all_ok(&network.run(steps));
    assert_eq!(log.lock().expect("no panic").requests, [0, 1]);
}

#[test]
fn sends_the_same_request_again() {
    let mut network = Network::new(31);
    let log = network.serve_each(PORT, |_| Reply::Close(OK.into(), RACE_IDLE));
    let put = Request::put(format!("http://{}/write", network.remote()))
        .header("x-key", "7")
        .body(Bytes::from_static(b"m v=1 5"))
        .expect("a valid request");
    let steps = race(&mut network, put);
    all_ok(&network.run(steps));
    let log = log.lock().expect("no panic");
    assert_eq!(log.requests, [0, 1]);
    let sent = super::text(&log.bytes[1]);
    assert!(sent.starts_with("PUT /write HTTP/1.1\r\n"), "{sent}");
    assert!(
        sent.contains(&format!("host: {}\r\n", network.remote())),
        "{sent}"
    );
    assert!(sent.contains("x-key: 7\r\n"), "{sent}");
    assert!(sent.ends_with("\r\n\r\nm v=1 5"), "{sent}");
}

fn post(network: &Network) -> Request<Bytes> {
    Request::post(format!("http://{}/write", network.remote()))
        .body(Bytes::from_static(b"m v=1 5"))
        .expect("a valid request")
}

#[test]
fn sends_a_post_again_when_the_close_came_before_the_write() {
    // At the first wait, the FIN reaches the client as the second send starts. At
    // the second, the client has read the FIN. In each, `hyper` does not write the
    // request and gives it back.
    for wait in [0, Span::MILLISECOND.nanos()].map(|late| RACE_IDLE.nanos() + late) {
        let mut network = Network::new(50);
        let log = network.serve_each(PORT, |_| Reply::Close(OK.into(), RACE_IDLE));
        let second = post(&network);
        let steps = race_at(&mut network, second, wait);
        all_ok(&network.run(steps));
        assert_eq!(log.lock().expect("no panic").requests, [0, 1], "{wait}");
    }
}

#[test]
fn fails_a_post_when_the_server_closed_the_idle_stream_first() {
    let mut network = Network::new(31);
    let log = network.serve_each(PORT, |_| Reply::Close(OK.into(), RACE_IDLE));
    let second = post(&network);
    let steps = race(&mut network, second);
    let outcomes = network.run(steps);
    all_ok(&outcomes[..1]);
    let error = outcomes[1].as_ref().expect_err("the stream closed");
    assert!(matches!(error, Error::Protocol(_)), "{error:?}");
    assert_eq!(
        error.to_string(),
        "the exchange failed: connection closed before message completed"
    );
    assert_eq!(log.lock().expect("no panic").requests, [0]);
}

/// The digest of a run with two idle streams that one send drops at once.
fn digest(seed: u64) -> u64 {
    let mut network = Network::new(seed);
    for port in PORT..PORT + 3 {
        network.serve_each(port, ok);
    }
    let mut steps = sends(&network, PORT, 1);
    steps.extend(sends(&network, PORT + 1, 1));
    steps.push(Step::Wait(OVER));
    steps.extend(sends(&network, PORT + 2, 1));
    all_ok(&network.run(steps));
    network.sim.digest()
}

#[test]
fn two_runs_of_one_seed_give_the_same_trace() {
    assert_eq!(digest(30), digest(30));
}

#[test]
fn fails_a_get_after_a_broken_response() {
    let mut network = Network::new(60);
    let log = network.serve_each(PORT, |n| match n {
        1 => Reply::Bytes("HTTP/1.1 abc\r\n\r\n".into()),
        _ => ok(n),
    });
    let outcomes = network.run(sends(&network, PORT, 2));
    all_ok(&outcomes[..1]);
    let error = outcomes[1].as_ref().expect_err("a bad status");
    assert_eq!(
        error.to_string(),
        "the exchange failed: invalid HTTP status-code parsed"
    );
    assert_eq!(
        log.lock().expect("no panic under the lock").requests,
        [0, 0]
    );
}

#[test]
fn fails_a_get_after_part_of_a_response() {
    let mut network = Network::new(61);
    let log = network.serve_each(PORT, |n| match n {
        1 => Reply::Close("HTTP/1.1 200 OK\r\n".into(), Span::ZERO),
        _ => ok(n),
    });
    let outcomes = network.run(sends(&network, PORT, 2));
    all_ok(&outcomes[..1]);
    let error = outcomes[1].as_ref().expect_err("a partial response");
    assert_eq!(
        error.to_string(),
        "the exchange failed: connection closed before message completed"
    );
    assert_eq!(
        log.lock().expect("no panic under the lock").requests,
        [0, 0]
    );
}
