use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use wire::hub::client::{REFUSED, Refusal};

use crate::client::{self, reach};

use super::*;

/// What [`reach`] gives on a shard of `host`, once it ends.
type Reached = Arc<Mutex<Option<Result<(), client::Error>>>>;

/// What [`reach`] dials with on `host`, with the tasks of a shard.
fn reach_config(host: &sim::node::Node, tasks: env::tasks::Tasks) -> client::Config {
    client::Config {
        files: host.files(),
        net: host.net(),
        clock: host.clock(),
        entropy: host.entropy(),
        tasks,
    }
}

/// Calls [`reach`] on a shard of `host`.
fn reach_on(host: &sim::node::Node) -> Reached {
    let out = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&out);
    shard(host, move |host, tasks| async move {
        let reached = reach(reach_config(&host, tasks)).await;
        *slot.lock().expect("no test thread panicked") = Some(reached.map(drop));
    });
    out
}

/// What `reached` holds once `sim` has run for a minute.
fn after_a_minute(sim: &mut sim::Sim, reached: &Reached) -> Result<(), client::Error> {
    assert_eq!(sim.run_for(Span::MINUTE), Ok(()));
    let reached = reached
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    reached.expect("reach ends in a minute")
}

/// What [`reach`] gave on a shard of `host`, once `sim` has run for a minute. A node
/// on `host` can run meanwhile.
fn reached(sim: &mut sim::Sim, host: &sim::node::Node) -> Result<(), client::Error> {
    let reached = reach_on(host);
    after_a_minute(sim, &reached)
}

/// Starts a node on `host`, which [`keyed`] made, at `listen`, with `region`.
fn start(host: &sim::node::Node, listen: SocketAddr, region: Region) -> Node {
    Node::start(Config {
        listen,
        region,
        ..config(host, Size::MEBIBYTE, Box::new(heap))
    })
}

/// Starts a node of the root region on a host that [`keyed`] made, runs it for a
/// second, and crashes it, which leaves `address` and `admin.key`.
fn crashed(sim: &mut sim::Sim) -> sim::node::Node {
    let host = keyed(sim, 2);
    let node = start(&host, listen(&host), Region::Stored);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    sim.crash(&host, sim::Crash::Process);
    drop(node);
    host
}

#[test]
fn reach_on_a_directory_where_no_node_ran_gives_not_running() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    assert_eq!(
        reached(&mut sim, &host),
        Err(client::Error::Local(sector::local::Error::NotRunning))
    );
}

#[test]
fn reach_after_a_clean_stop_gives_not_running() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = start(&host, listen(&host), Region::Stored);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    node.stop();
    assert_eq!(sim.run(), Ok(()));
    assert_eq!(node.join(), Ok(()));
    assert_eq!(
        reached(&mut sim, &host),
        Err(client::Error::Local(sector::local::Error::NotRunning))
    );
}

/// A node with no region writes no `admin.key`.
#[test]
fn reach_on_a_node_with_no_admin_key_gives_key() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = start(&host, listen(&host), Region::Absent);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    assert_eq!(reached(&mut sim, &host), Err(client::Error::Key));
    node.stop();
    assert_eq!(sim.run(), Ok(()));
}

#[test]
fn reach_with_an_admin_key_that_no_node_wrote_gives_key() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = crashed(&mut sim);
    let written = sim.run_on(&host, |host, _| async move {
        let files = host.files();
        let mode = env::files::Mode::Write;
        let file = files.open(Path::new(sector::admin::FILE), mode).await?;
        let pool = pool();
        let block = pool.copy(b"x").expect("a block");
        file.write_at(0, &[block]).await
    });
    assert_eq!(written, Ok(Ok(())));
    assert_eq!(reached(&mut sim, &host), Err(client::Error::Key));
}

#[test]
fn reach_on_an_address_of_another_version_gives_version() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = crashed(&mut sim);
    let written = sim.run_on(&host, |host, _| async move {
        let files = host.files();
        let path = Path::new(sector::address::FILE);
        files.remove(path).await?;
        let mut body = b"foundation/address/2".to_vec();
        body.resize(120, 0);
        body.extend_from_slice(&crc32c::crc32c(&body).to_le_bytes());
        let mode = env::files::Mode::Create { len: 124 };
        let file = files.open(path, mode).await?;
        let pool = pool();
        let block = pool.copy(&body).expect("a block");
        file.write_at(0, &[block]).await
    });
    assert_eq!(written, Ok(Ok(())));
    let version = client::Error::Local(sector::local::Error::Version {
        file: "address",
        found: b'2',
        expected: b'1',
    });
    assert_eq!(reached(&mut sim, &host), Err(version));
}

#[test]
fn reach_whose_read_of_address_fails_gives_the_file_error() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = crashed(&mut sim);
    let operation = env::files::Operation::ReadAt;
    host.fail_file(Path::new(sector::address::FILE), operation);
    let error = env::files::Error::Io {
        path: PathBuf::from("address"),
        operation,
        code: 5,
    };
    let error = client::Error::Local(sector::local::Error::Files(error));
    assert_eq!(reached(&mut sim, &host), Err(error));
}

#[test]
fn reach_whose_read_of_the_admin_key_fails_gives_the_file_error() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = crashed(&mut sim);
    let operation = env::files::Operation::ReadAt;
    host.fail_file(Path::new(sector::admin::FILE), operation);
    let error = env::files::Error::Io {
        path: PathBuf::from("admin.key"),
        operation,
        code: 5,
    };
    let error = client::Error::Local(sector::local::Error::Files(error));
    assert_eq!(reached(&mut sim, &host), Err(error));
}

#[test]
fn reach_makes_no_file() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let list = |sim: &mut sim::Sim| {
        sim.run_on(&host, |host, _| async move {
            host.files().list(Path::new("")).await
        })
    };
    let before = list(&mut sim);
    assert_eq!(
        reached(&mut sim, &host),
        Err(client::Error::Local(sector::local::Error::NotRunning))
    );
    assert_eq!(list(&mut sim), before);
}

#[test]
fn reach_after_a_crash_gives_unanswered_at_the_address() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = crashed(&mut sim);
    let reached = reached(&mut sim, &host);
    let at = listen(&host);
    let error = transport::Error::Unreachable {
        peer: KEY.public(),
        attempts: vec![(Address::Udp(at), transport::Error::TimedOut)],
    };
    assert_eq!(reached, Err(client::Error::Unanswered { at, error }));
}

/// The node admits the hello of its first admin at its bound address.
#[test]
fn reach_on_a_running_node_dials_its_bound_address() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = start(&host, listen(&host), Region::Stored);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    assert_eq!(reached(&mut sim, &host), Ok(()));
    node.stop();
    assert_eq!(sim.run(), Ok(()));
}

/// `sim` has no loopback, so the dial times out where an OS gives the reply of the
/// node (#2521).
#[test]
fn reach_dials_the_loopback_ip_of_the_family_for_an_unspecified_ip() {
    let families = [
        (
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V4(Ipv4Addr::LOCALHOST),
        ),
        (
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ),
    ];
    for (any, loopback) in families {
        let mut sim = sim::Sim::new(sim::Config::default());
        let host = keyed(&mut sim, 2);
        let node = start(&host, SocketAddr::new(any, PORT), Region::Stored);
        assert_eq!(sim.run_for(Span::SECOND), Ok(()));
        let reached = reached(&mut sim, &host);
        let at = SocketAddr::new(loopback, PORT);
        let error = transport::Error::Unreachable {
            peer: KEY.public(),
            attempts: vec![(Address::Udp(at), transport::Error::TimedOut)],
        };
        assert_eq!(reached, Err(client::Error::Unanswered { at, error }));
        node.stop();
        assert_eq!(sim.run(), Ok(()));
    }
}

/// A peer at the node's address that finishes the hub stream with no challenge.
#[test]
fn reach_whose_hub_stream_ends_with_no_challenge_gives_session() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = crashed(&mut sim);
    let at = listen(&host);
    shard(&host, move |host, tasks| async move {
        let (transport, _pool) = transport_at(&host, tasks, KEY, at);
        let session = transport.accept().await.expect("a session");
        let incoming = session.accept().await.expect("a stream");
        let mut sender = incoming.sender.expect("the hello stream goes both ways");
        sender.finish().expect("the stream finishes");
        host.clock().sleep(Span::MINUTE).await;
    });
    assert_eq!(
        reached(&mut sim, &host),
        Err(client::Error::Session {
            at,
            error: program::Error::Unanswered,
        })
    );
}

/// The socket of the program breaks while it dials a crashed node, so no session
/// exists.
#[test]
fn reach_whose_socket_breaks_during_the_dial_gives_transport() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = crashed(&mut sim);
    let reached = reach_on(&host);
    assert_eq!(
        sim.run_for(Span::from_nanos(100 * Span::MILLISECOND.nanos())),
        Ok(())
    );
    // The simulated net gives the first bind its first ephemeral port.
    host.fail_udp(SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 49_152));
    let error = transport::Error::Network {
        error: env::net::Error::Io { code: 5 },
    };
    assert_eq!(
        after_a_minute(&mut sim, &reached),
        Err(client::Error::Transport(error))
    );
}

/// A peer at the node's address that closes each session with a refusal.
#[test]
fn reach_whose_hello_the_node_refuses_gives_refused() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = crashed(&mut sim);
    let at = listen(&host);
    shard(&host, move |host, tasks| async move {
        let (transport, _pool) = transport_at(&host, tasks, KEY, at);
        let session = transport.accept().await.expect("a session");
        session.close(Code(REFUSED));
        host.clock().sleep(Span::MINUTE).await;
    });
    assert_eq!(
        reached(&mut sim, &host),
        Err(client::Error::Refused(Refusal::Refused))
    );
}

/// A request of `plan` of `plant.hcl`, which defines the index `plant.time` and homes
/// it on `edge`, the node.
const PLAN: &[u8] = concat!(
    r#"{"plan":{"files":[{"path":"plant.hcl","text":""#,
    r#"channel \"plant.time\" { kind = \"index\" }\n"#,
    r#"placement \"plant.home\" {\n  select = \"plant.*\"\n  home = \"edge\"\n}\n"#,
    "\"}]}}\n",
)
.as_bytes();

/// The replies that the client of [`reach`] on a shard of `host` gets to [`PLAN`],
/// then to a request of `apply` of the plan file that follows the first line of the
/// first reply, once `sim` has run for a minute.
fn plan_and_apply(sim: &mut sim::Sim, host: &sim::node::Node) -> [Vec<u8>; 2] {
    let out = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&out);
    shard(host, move |host, tasks| async move {
        let client = reach(reach_config(&host, tasks)).await;
        let client = client.expect("the node admits its first admin");
        let planned = client.request(PLAN).await.expect("a reply of plan");
        let end = planned.iter().position(|&byte| byte == b'\n');
        let plan = &planned[end.expect("a first line") + 1..];
        let apply = [&b"{\"apply\":{}}\n"[..], plan].concat();
        let applied = client.request(&apply).await.expect("a reply of apply");
        *slot.lock().expect("no test thread panicked") = Some([planned, applied]);
    });
    assert_eq!(sim.run_for(Span::MINUTE), Ok(()));
    let replies = out.lock().unwrap_or_else(PoisonError::into_inner).take();
    replies.expect("the requests end in a minute")
}

/// The definitions of the spec that `node` uses, once `sim` has run for a second.
fn used(
    sim: &mut sim::Sim,
    node: &Node,
) -> BTreeMap<types::name::Name, spec::definition::Definition> {
    let out = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&out);
    node.operate(move |ops| async move {
        let definitions = (*ops.mesh().used().definitions).clone();
        *slot.lock().expect("no test thread panicked") = Some(definitions);
    });
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    let used = out.lock().unwrap_or_else(PoisonError::into_inner).take();
    used.expect("the operation ends in a second")
}

/// The node answers each request of its first admin with the reply of `ops::serve`:
/// the apply of the plan puts `plant.time` in the spec in use.
#[test]
fn a_program_plans_and_applies_on_the_node() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = start(&host, listen(&host), Region::Stored);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    let label = spec::definition::Kind::Channel.key("plant.time").unwrap();
    assert!(!used(&mut sim, &node).contains_key(&label));
    let [planned, applied] = plan_and_apply(&mut sim, &host);
    let end = planned.iter().position(|&byte| byte == b'\n').unwrap();
    let base = concat!(
        r#"{"version":0,"#,
        r#""root":"3ae069cacea966c3a682f4421d2cba2acc91d8db6748381bb44ee7d7a3ccd300"}"#,
    );
    let changes = concat!(
        r#"[{"action":"add","kind":"channel","name":"plant.time","#,
        r#""place":{"file":"plant.hcl","line":1,"column":9}},"#,
        r#"{"action":"add","kind":"placement","name":"plant.home","#,
        r#""place":{"file":"plant.hcl","line":2,"column":11}}]"#,
    );
    let reserved = concat!(
        r#"[{"name":"@admin","kind":"access","key":"@admin.@access"},"#,
        r#"{"name":"@admin","kind":"subject","key":"@admin.@subject"}]"#,
    );
    let output = format!(
        "{{\"output\":{{\"base\":{base},\"changes\":{changes},\
         \"homes\":{{\"plant.time\":\"edge\"}},\"reserved\":{reserved},\
         \"added\":2,\"changed\":0,\"removed\":0}}}}\n"
    );
    assert_eq!(text(&planned[..=end]), output);
    let applied_output = concat!(
        r#"{"output":{"pointer":{"version":1,"root":"#,
        r#""5366edbb244d9401ca0af4e03ddb9523067d572ced0cb177566526f9f04d3691"},"#,
        r#""added":2,"changed":0,"removed":0,"homes":1}}"#,
        "\n",
    );
    assert_eq!(text(&applied), applied_output);
    assert!(used(&mut sim, &node).contains_key(&label));
    node.stop();
    assert_eq!(sim.run(), Ok(()));
}

/// `bytes` as text, for an exact compare.
fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("a reply is UTF-8")
}

/// The key of `plant.operator`, which no founding lists.
const OPERATOR: PrivateKey = PrivateKey([5; 32]);

/// The reply to [`PLAN`] of `plant.operator` with [`OPERATOR`] on a shard of `host`,
/// at the node whose address `host` holds, once `sim` has run for a minute.
fn operator(
    sim: &mut sim::Sim,
    host: &sim::node::Node,
) -> Result<Vec<u8>, program::Error> {
    let out = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&out);
    shard(host, move |host, tasks| async move {
        let files = host.files();
        let local = sector::local::read(&files).await.expect("a node runs");
        let (transport, pool) = program(&host, tasks.clone());
        let frames = super::pool();
        let config = program::Config {
            via: local.key,
            node: local.public_key,
            addresses: vec![Address::Udp(local.address)],
            subject: "plant.operator".parse().unwrap(),
            key: OPERATOR,
            clock: host.clock(),
            entropy: host.entropy(),
            tasks,
            pool,
            frames: Rc::clone(&frames),
            reads: frames,
        };
        let client = program::Session::connect(&transport, config).await;
        let reply = match client {
            Ok(client) => client.request(PLAN).await,
            Err(error) => Err(error),
        };
        *slot.lock().expect("no test thread panicked") = Some(reply);
    });
    assert_eq!(sim.run_for(Span::MINUTE), Ok(()));
    let reply = out.lock().unwrap_or_else(PoisonError::into_inner).take();
    reply.expect("the request ends in a minute")
}

/// The hub takes the rules of each later spec in use: a hello of a subject that no
/// spec lists is refused, and the same hello after a spec adds the subject is
/// admitted. Its request runs as that subject, which no policy grants `plan`.
#[test]
fn a_hello_of_a_subject_that_a_later_spec_adds_is_admitted() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let host = keyed(&mut sim, 2);
    let node = start(&host, listen(&host), Region::Stored);
    assert_eq!(sim.run_for(Span::SECOND), Ok(()));
    assert_eq!(
        operator(&mut sim, &host),
        Err(program::Error::Refused(Refusal::Refused))
    );
    let applied = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&applied);
    node.operate(move |ops| async move {
        let mesh = ops.mesh();
        let used = mesh.used();
        let mut definitions = (*used.definitions).clone();
        let subject = spec::subject::Subject::new(vec![OPERATOR.public()]).unwrap();
        let label = spec::definition::Kind::Subject
            .key("plant.operator")
            .unwrap();
        definitions.insert(label, spec::definition::Definition::Subject(subject));
        let base = used.pointer.expect("a spec in use");
        let applied = mesh.apply(base, definitions, BTreeMap::new()).await;
        *slot.lock().expect("no test thread panicked") = Some(applied.map(drop));
    });
    assert_eq!(sim.run_for(Span::MINUTE), Ok(()));
    let applied = applied
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    assert_eq!(applied, Some(Ok(())));
    let reply = operator(&mut sim, &host).expect("admitted");
    let denied = concat!(
        r#"{"errors":[{"code":"ops.denied","message":"no access policy gives "#,
        r#"`plant.operator` the action `plan` on `plant.home.@placement`","#,
        r#""fix":"Ask an admin of the region for an access policy that allows it","#,
        r#""notes":[]}],"status":1}"#,
        "\n",
    );
    assert_eq!(text(&reply), denied);
    node.stop();
    assert_eq!(sim.run(), Ok(()));
}
