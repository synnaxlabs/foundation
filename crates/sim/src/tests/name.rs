//! Tests of the name lookups of a run through `env::net`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker};

use env::net::Error as Net;
use types::time::{Monotonic, Span};

use super::{millis, pair, shard, sim};
use crate::name::{Answer, Config};
use crate::{Sim, link, node};

/// The answer to a lookup, and the time it took on the clock of its node.
type Lookup = (Result<Vec<SocketAddr>, Net>, Span);

const HOST: &str = "historian.local";

fn v4(host: u8) -> IpAddr {
    IpAddr::V4(Ipv4Addr::new(10, 0, 0, host))
}

/// `ip` with port 4433.
fn at(ip: IpAddr) -> SocketAddr {
    SocketAddr::new(ip, 4433)
}

/// A name with `addresses` and no delay.
fn addresses(addresses: Vec<IpAddr>) -> Config {
    Config {
        answer: Answer::Addresses(addresses),
        ..Config::default()
    }
}

/// A run with one node.
fn one() -> (Sim, node::Node) {
    let mut sim = sim(0);
    let node = sim.node(node::Config::default());
    (sim, node)
}

/// Looks up [`HOST`] with port 4433 on `node`, and runs until the lookup ends.
fn lookup(sim: &mut Sim, node: &node::Node) -> Lookup {
    let lookup = sim.run_on(node, |node, _| async move {
        let clock = node.clock();
        let start = clock.now();
        let answer = node.net().resolve(HOST, 4433).await;
        (answer, clock.now() - start)
    });
    lookup.unwrap()
}

/// The error of a lookup of [`HOST`] that finds no address.
fn not_found() -> Net {
    Net::NotFound {
        host: HOST.to_owned(),
    }
}

#[test]
fn a_name_gives_its_addresses_in_order_with_the_port() {
    let (mut sim, node) = one();
    let v6 = IpAddr::V6(Ipv6Addr::new(0xfd00, 0, 0, 0, 0, 0, 0, 2));
    sim.name(HOST, addresses(vec![v6, v4(2)]));
    let found = vec![at(v6), at(v4(2))];
    assert_eq!(lookup(&mut sim, &node), (Ok(found), Span::ZERO));
}

#[test]
fn a_name_that_no_call_gave_has_no_address() {
    let (mut sim, node) = one();
    sim.name("pump.local", addresses(vec![v4(2)]));
    assert_eq!(lookup(&mut sim, &node), (Err(not_found()), Span::ZERO));
}

#[test]
fn a_name_with_no_addresses_has_no_address() {
    let (mut sim, node) = one();
    sim.name(HOST, addresses(vec![]));
    assert_eq!(lookup(&mut sim, &node), (Err(not_found()), Span::ZERO));
}

#[test]
fn a_failed_lookup_gives_eagain_after_its_delay() {
    let (mut sim, node) = one();
    let config = Config {
        answer: Answer::Failed,
        delay: millis(5),
    };
    sim.name(HOST, config);
    let failed = Net::Io { code: 11 };
    assert_eq!(lookup(&mut sim, &node), (Err(failed), millis(5)));
}

#[test]
fn a_lookup_takes_the_delay_of_its_name() {
    let (mut sim, node) = one();
    let config = Config {
        delay: millis(50),
        ..addresses(vec![v4(2)])
    };
    sim.name(HOST, config);
    assert_eq!(lookup(&mut sim, &node), (Ok(vec![at(v4(2))]), millis(50)));
}

#[test]
fn a_lookup_gives_the_answer_of_its_start() {
    let (mut sim, node) = one();
    let config = Config {
        delay: millis(50),
        ..addresses(vec![v4(2)])
    };
    sim.name(HOST, config);
    let answer = Arc::new(Mutex::new(None));
    let (slot, net) = (Arc::clone(&answer), node.net());
    let start = node.shards().start(shard("lookup"), move |_| async move {
        *slot.lock().unwrap() = Some(net.resolve(HOST, 4433).await);
    });
    drop(start.unwrap());
    sim.run_for(millis(10)).unwrap();
    sim.name(HOST, addresses(vec![v4(3)]));
    sim.run().unwrap();
    assert_eq!(*answer.lock().unwrap(), Some(Ok(vec![at(v4(2))])));
    assert_eq!(lookup(&mut sim, &node), (Ok(vec![at(v4(3))]), Span::ZERO));
}

#[test]
fn a_name_matches_in_any_ascii_case() {
    let (mut sim, node) = one();
    sim.name("Historian.LOCAL", addresses(vec![v4(2)]));
    assert_eq!(lookup(&mut sim, &node), (Ok(vec![at(v4(2))]), Span::ZERO));
    let found = sim.run_on(&node, |node, _| async move {
        node.net().resolve("HISTORIAN.Local", 4433).await
    });
    assert_eq!(found, Ok(Ok(vec![at(v4(2))])));
}

#[test]
fn a_name_matches_with_or_without_a_final_dot() {
    let (mut sim, node) = one();
    sim.name("historian.local.", addresses(vec![v4(2)]));
    assert_eq!(lookup(&mut sim, &node), (Ok(vec![at(v4(2))]), Span::ZERO));
    sim.name(HOST, addresses(vec![v4(3)]));
    let found = sim.run_on(&node, |node, _| async move {
        node.net().resolve("historian.local.", 4433).await
    });
    assert_eq!(found, Ok(Ok(vec![at(v4(3))])));
}

#[test]
fn each_node_gets_the_answer_through_a_partition() {
    let cut = link::Config {
        loss: 1.0,
        ..link::Config::default()
    };
    let (mut sim, a, b) = pair(0, cut);
    let config = Config {
        delay: millis(5),
        ..addresses(vec![v4(2)])
    };
    sim.name(HOST, config);
    for node in [&a, &b] {
        assert_eq!(lookup(&mut sim, node), (Ok(vec![at(v4(2))]), millis(5)));
    }
}

#[test]
fn a_lookup_that_ends_past_the_clock_never_answers() {
    let mut sim = sim(0);
    let node = sim.node(node::Config {
        monotonic: Monotonic(u64::MAX / 2 + 10),
        ..node::Config::default()
    });
    let config = Config {
        delay: Span::from_nanos(i64::MAX),
        ..addresses(vec![v4(2)])
    };
    sim.name(HOST, config);
    let answer = Arc::new(Mutex::new(None));
    let (slot, net) = (Arc::clone(&answer), node.net());
    let start = node.shards().start(shard("lookup"), move |_| async move {
        *slot.lock().unwrap() = Some(net.resolve(HOST, 4433).await);
    });
    drop(start.unwrap());
    sim.run_for(millis(10)).unwrap();
    assert_eq!(*answer.lock().unwrap(), None);
}

#[test]
#[should_panic(expected = "10.0.0.2 is an IP literal, which no lookup reads")]
fn a_name_that_is_an_ip_literal_panics() {
    let (mut sim, _) = one();
    sim.name("10.0.0.2", addresses(vec![v4(3)]));
}

#[test]
#[should_panic(expected = "[fd00::2] is an IP literal, which no lookup reads")]
fn a_name_that_is_an_ipv6_literal_in_brackets_panics() {
    let (mut sim, _) = one();
    sim.name("[fd00::2]", addresses(vec![v4(3)]));
}

#[test]
#[should_panic(expected = "10.0.0.2. is an IP literal, which no lookup reads")]
fn a_name_that_is_an_ip_literal_with_a_final_dot_panics() {
    let (mut sim, _) = one();
    sim.name("10.0.0.2.", addresses(vec![v4(3)]));
}

#[test]
fn the_empty_host_is_not_the_root() {
    let (mut sim, node) = one();
    sim.name(".", addresses(vec![v4(2)]));
    let found = sim.run_on(&node, |node, _| async move {
        node.net().resolve("", 4433).await
    });
    let host = String::new();
    assert_eq!(found, Ok(Err(Net::NotFound { host })));
}

#[test]
fn an_ipv4_address_in_brackets_is_a_name() {
    let (mut sim, node) = one();
    sim.name("[10.0.0.2]", addresses(vec![v4(3)]));
    let found = sim.run_on(&node, |node, _| async move {
        node.net().resolve("[10.0.0.2]", 4433).await
    });
    assert_eq!(found, Ok(Ok(vec![at(v4(3))])));
}

#[test]
#[should_panic(expected = "a lookup needs a thread that the sim started")]
fn a_lookup_outside_the_sim_panics() {
    let (_sim, node) = one();
    let net = node.net();
    let mut lookup = pin!(net.resolve(HOST, 4433));
    let mut cx = Context::from_waker(Waker::noop());
    drop(lookup.as_mut().poll(&mut cx));
}

#[test]
fn a_literal_needs_no_name() {
    let (mut sim, node) = one();
    let found = sim.run_on(&node, |node, _| async move {
        node.net().resolve("10.0.0.2", 4433).await
    });
    assert_eq!(found, Ok(Ok(vec![at(v4(2))])));
}

#[test]
#[should_panic(expected = "the lookup of historian.local takes a negative delay of")]
fn a_negative_delay_panics() {
    let (mut sim, _) = one();
    let config = Config {
        delay: Span::from_nanos(-1),
        ..Config::default()
    };
    sim.name(HOST, config);
}
