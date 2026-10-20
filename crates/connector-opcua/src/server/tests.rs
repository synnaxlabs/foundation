use std::future::{pending, poll_fn};
use std::net::SocketAddr;
use std::sync::{Arc, mpsc};
use std::task::Poll;

use ::sim::{Crash, Sim, node};
use env::net::tcp;
use types::time::Span;

use super::{READ, Server, Variable};
use crate::connection::{Manager, OPTIONS};
use crate::event;
use crate::node::{Node, ns0};
use crate::unwind::caught;
use crate::value::Value;

#[test]
fn an_add_of_a_node_of_the_number_0_panics_and_frees_the_port() {
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    sim.run_on(&node, move |node, _| async move {
        let mut rng = node.entropy().rng();
        let local = SocketAddr::new(node.addresses()[0], 4840);
        let manager = Manager::listening(
            event::Time::monotonic(node.clock()),
            node.net(),
            local,
            4,
            &mut rng,
        )
        .expect("the port is free");
        let zero: Node = "ns=1;i=0".parse().expect("a node");
        let add = |server: &mut Server| server.add_scalar(&zero, Value::F64(1.0));
        let panic = caught(Server::serve(manager, 1, None, add, async {}))
            .await
            .expect_err("the serve panics");
        assert_eq!(
            panic.downcast_ref::<String>().map(String::as_str),
            Some("ns=1;i=0: open62541 adds a node of the number 0 at a free number")
        );
        Manager::listening(
            event::Time::monotonic(node.clock()),
            node.net(),
            local,
            4,
            &mut rng,
        )
        .expect("the serve frees the port");
    })
    .expect("the run ends");
}

#[test]
fn a_drop_of_a_serve_that_waits_closes_the_server_and_frees_the_port() {
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    sim.run_on(&node, move |node, _| async move {
        let mut rng = node.entropy().rng();
        let local = SocketAddr::new(node.addresses()[0], 4840);
        let listen = |rng: &mut _| {
            Manager::listening(
                event::Time::monotonic(node.clock()),
                node.net(),
                local,
                4,
                rng,
            )
        };
        let manager = listen(&mut rng).expect("the port is free");
        let mut serve =
            Box::pin(Server::serve(manager, 1, None, |_| {}, pending::<()>()));
        let waits = poll_fn(|cx| Poll::Ready(serve.as_mut().poll(cx).is_pending()));
        assert!(waits.await, "the serve waits on its end");
        drop(serve);
        listen(&mut rng).expect("the drop frees the port");
    })
    .expect("the run ends");
}

/// A crash drops the future of the serve where no I/O or sleep can run, while a
/// stream is open on the server.
#[cfg(feature = "sim")]
#[test]
fn a_crash_while_a_stream_is_open_frees_the_server() {
    let mut sim = Sim::new(::sim::Config::default());
    let (device, peer) = (
        sim.node(node::Config::default()),
        sim.node(node::Config::default()),
    );
    let local = SocketAddr::new(device.addresses()[0], 4840);
    let held = Arc::new(());
    let (served, node) = (Arc::clone(&held), device.clone());
    let started = device.shards().start(shard("serve"), move |_| async move {
        let manager = Manager::listening(
            event::Time::monotonic(node.clock()),
            node.net(),
            local,
            4,
            &mut node.entropy().rng(),
        )
        .expect("the port is free");
        let source: Node = "ns=1;i=7".parse().expect("a node");
        let add = |server: &mut Server| {
            server.add_source(&source, move || {
                let () = *served;
                (Value::I32(1), types::quality::Quality::GOOD)
            });
        };
        let served = Server::serve(manager, 1, None, add, pending::<()>());
        served.await.expect("the listener does not fail");
    });
    drop(started.expect("the shard starts"));
    let net = peer.net();
    let config = tcp::Config {
        remote: local,
        options: OPTIONS,
    };
    let started = peer.shards().start(shard("dial"), move |_| async move {
        let _stream = net.connect(&config).await.expect("the connect works");
        pending::<()>().await;
    });
    drop(started.expect("the shard starts"));
    sim.run_for(Span::SECOND).expect("the run goes on");
    assert_eq!(Arc::strong_count(&held), 2, "the server holds its source");
    sim.crash(&device, Crash::Power);
    assert_eq!(Arc::strong_count(&held), 1, "the crash frees the source");
}

/// The close at the end of a serve closes each stream, so the peer reads the end of
/// the stream and no reset.
#[test]
fn the_end_of_a_serve_closes_each_open_stream() {
    let mut sim = Sim::new(::sim::Config::default());
    let (device, peer) = (
        sim.node(node::Config::default()),
        sim.node(node::Config::default()),
    );
    let local = SocketAddr::new(device.addresses()[0], 4840);
    let node = device.clone();
    let started = device.shards().start(shard("serve"), move |_| async move {
        let manager = Manager::listening(
            event::Time::monotonic(node.clock()),
            node.net(),
            local,
            4,
            &mut node.entropy().rng(),
        )
        .expect("the port is free");
        let served =
            Server::serve(manager, 1, None, |_| {}, node.clock().sleep(Span::SECOND));
        served.await.expect("the listener does not fail");
    });
    drop(started.expect("the shard starts"));
    let (net, (send, read)) = (peer.net(), mpsc::channel());
    let config = tcp::Config {
        remote: local,
        options: OPTIONS,
    };
    let started = peer.shards().start(shard("dial"), move |_| async move {
        let mut stream = net.connect(&config).await.expect("the connect works");
        let mut buffer = [0; 64];
        let read = poll_fn(|cx| stream.poll_read(cx, &mut buffer)).await;
        send.send(read).expect("the test waits");
    });
    drop(started.expect("the shard starts"));
    sim.run_for(Span::from_nanos(2 * Span::SECOND.nanos()))
        .expect("the run goes on");
    assert_eq!(read.try_recv().expect("the read ends"), Ok(0));
}

fn shard(name: &str) -> env::shards::Config {
    env::shards::Config {
        name: name.into(),
        core: None,
    }
}

/// A value whose mask names no built-in type (63), or that ends before its Double,
/// is not a `Variant`.
#[test]
fn an_add_of_a_value_that_is_not_a_variant_panics() {
    for value in [vec![63], vec![11, 0, 0]] {
        let mut sim = Sim::new(::sim::Config::default());
        let node = sim.node(node::Config::default());
        let message = sim
            .run_on(&node, move |node, _| async move {
                let mut rng = node.entropy().rng();
                let local = SocketAddr::new(node.addresses()[0], 4840);
                let manager = Manager::listening(
                    event::Time::monotonic(node.clock()),
                    node.net(),
                    local,
                    4,
                    &mut rng,
                )
                .expect("the port is free");
                let variable = Variable {
                    node: "ns=1;i=7".parse().expect("a node"),
                    parent: Node::ns0(ns0::OBJECTS),
                    reference: ns0::ORGANIZES,
                    name: c"v",
                    data_type: Node::ns0(ns0::BASE_DATA_TYPE),
                    rank: -1,
                    access: READ,
                    value: Some(value),
                };
                let add = |server: &mut Server| server.add_variable(&variable);
                let panic = caught(Server::serve(manager, 1, None, add, async {}))
                    .await
                    .expect_err("the serve panics");
                panic.downcast_ref::<String>().cloned()
            })
            .expect("the run ends")
            .expect("a message");
        assert!(
            message.starts_with(
                "assertion `left == right` failed: open62541 refused the node"
            ),
            "{message}"
        );
    }
}

#[cfg(feature = "sim")]
#[test]
fn a_source_is_a_variable_of_the_data_type_base_data_type() {
    let mut sim = Sim::new(::sim::Config::default());
    let node = sim.node(node::Config::default());
    sim.run_on(&node, move |node, _| async move {
        let mut rng = node.entropy().rng();
        let local = SocketAddr::new(node.addresses()[0], 4840);
        let manager = Manager::listening(
            event::Time::monotonic(node.clock()),
            node.net(),
            local,
            4,
            &mut rng,
        )
        .expect("the port is free");
        let source: Node = "ns=1;i=7".parse().expect("a node");
        let add = |server: &mut Server| {
            server
                .add_source(&source, || (Value::I32(1), types::quality::Quality::GOOD));
            assert_eq!(server.data_type(&source), ns0::BASE_DATA_TYPE);
        };
        let served = Server::serve(manager, 1, None, add, async {});
        served.await.expect("the listener does not fail");
    })
    .expect("the run ends");
}

#[test]
fn doubles_is_the_encoding_of_an_array_of_the_doubles_1_5_and_2_5() {
    let expected = [
        &[0x8B, 2, 0, 0, 0][..],
        &1.5_f64.to_le_bytes(),
        &2.5_f64.to_le_bytes(),
    ]
    .concat();
    assert_eq!(super::doubles(), expected);
}

#[test]
fn a_scalar_takes_a_type_of_at_most_63() {
    assert_eq!(super::scalar(63, &[1]), [63, 1]);
    let panic = std::panic::catch_unwind(|| super::scalar(64, &[]))
        .expect_err("64 is past the mask");
    assert_eq!(
        panic.downcast_ref::<String>().map(String::as_str),
        Some("a built-in type of at most 63")
    );
}
