//! Hub link sessions between two sim nodes: the program dials as an [`Agent`], and the
//! home serves each stream on `Link::serve`.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use hub::{Hub, Served, serve};
use spec::definition::{Definition, Kind};
use spec::subject::Subject;
use transport::{Address, Code, Session};
use types::name::{Name, Prefix};
use types::time::Span;

use crate::agent::{AGENT, Agent, SECOND, SUBJECT, THIRD, name};
use crate::net::{HOME, PORT, header, own_pool, public_key, transport};

/// How long the home holds a request before it replies.
pub(crate) const HOLD: Span = Span::from_nanos(50_000_000);

/// The rules of a root tree that lists `AGENT` for [`SUBJECT`], [`SECOND`], and
/// [`THIRD`].
pub(crate) fn rules() -> access::Rules {
    let tree: BTreeMap<Name, Definition> = [SUBJECT, SECOND, THIRD]
        .into_iter()
        .map(|subject| {
            let key = Kind::Subject.key(subject).expect("a subject key");
            let listed = Subject::new(vec![public_key(&AGENT)]).expect("a subject");
            (key, Definition::Subject(listed))
        })
        .collect();
    access::Rules::new([(Prefix::ROOT, &tree)])
}

/// What the home's link gave for one stream.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Got {
    Ended,
    /// A request, with the subject of its hello and its body.
    Request(Name, Vec<u8>),
}

/// Runs a client session of the hub that `open` gives on the home's node, for each of
/// `subjects`, from its own port of the program's node. The home keeps what `open`
/// gives with the hub until each session ends. Each stream is served as
/// [`serve_each`] serves it, and each agent admits a hello of its subject. Gives what
/// `serve` gave for each stream, in the order they ended.
pub(crate) fn sessions<const N: usize, T, P>(
    seed: u64,
    subjects: [&'static str; N],
    open: impl AsyncFnOnce(&sim::node::Node, &env::tasks::Tasks) -> (Hub, T)
    + Send
    + 'static,
    program: impl FnOnce([Agent; N]) -> P + Send + 'static,
) -> Vec<Result<Got, serve::Error>>
where
    P: Future<Output = ()> + 'static,
{
    let served = Arc::new(Mutex::new(Vec::new()));
    let kept = Arc::clone(&served);
    let home = move |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let (hub, kept_open) = open(&node, &tasks).await;
        hub.set_rules(rules());
        let transport = transport(&node, &tasks, &own_pool(), HOME, 1 << 16);
        let accepted = Rc::new(Cell::new(0));
        let mut ends = Vec::new();
        for _ in 0..N {
            let session = transport.accept().await.expect("a session");
            let link = hub.link(session.clone());
            tasks.spawn(serve_each(
                &session,
                &link,
                &tasks,
                &node.clock(),
                &kept,
                &accepted,
            ));
            ends.push(session);
        }
        for session in ends {
            session.closed().await;
        }
        recorded(&node, &kept, accepted.get()).await;
        drop((hub, kept_open));
    };
    run_program(seed, home, move |node, tasks, at| async move {
        let mut agents = Vec::new();
        for subject in subjects {
            agents.push(Agent::dial(&node, tasks.clone(), at, name(subject)).await);
        }
        let ends: Vec<_> = agents.iter().map(|agent| agent.session.clone()).collect();
        program(agents.try_into().ok().expect("an agent for each session")).await;
        for session in ends {
            session.close(Code(0));
        }
        node.clock().sleep(Span::MILLISECOND).await;
    });
    std::mem::take(&mut *served.lock().expect("not poisoned"))
}

/// `asserted`, then the close of each stream still open when [`sessions`] closed its
/// `count` sessions: `stalled` requests and the hello stream of each session.
pub(crate) fn closed_after(
    mut asserted: Vec<Result<Got, serve::Error>>,
    count: usize,
    stalled: usize,
) -> Vec<Result<Got, serve::Error>> {
    let open = count + stalled;
    asserted.extend((0..open).map(|_| Err(serve::Error::Stream(closed_with(0)))));
    asserted
}

/// What `serve` gave for each stream, in the order they ended.
pub(crate) type Kept = Arc<Mutex<Vec<Result<Got, serve::Error>>>>;

/// Serves each stream of `session` on `link` in its own task through [`answer`], and
/// pushes what it gives to `kept`. Counts each stream in `accepted`. Ends when the
/// session ends.
pub(crate) fn serve_each(
    session: &Session,
    link: &hub::Link,
    tasks: &env::tasks::Tasks,
    clock: &env::clock::Clock,
    kept: &Kept,
    accepted: &Rc<Cell<usize>>,
) -> impl Future<Output = ()> + 'static {
    let (session, link, tasks, clock, kept, accepted) = (
        session.clone(),
        link.clone(),
        tasks.clone(),
        clock.clone(),
        Arc::clone(kept),
        Rc::clone(accepted),
    );
    async move {
        while let Ok(mut incoming) = session.accept().await {
            accepted.set(accepted.get() + 1);
            let (link, kept, clock) = (link.clone(), Arc::clone(&kept), clock.clone());
            tasks.spawn(async move {
                header(&mut incoming).await;
                let got = answer(link.serve(incoming), &clock).await;
                kept.lock().expect("not poisoned").push(got);
            });
        }
    }
}

/// Waits until `kept` holds what `serve` gave for each of `accepted` streams. The end
/// of the home's future drops each serve task that has not yet recorded.
///
/// # Panics
///
/// When a second passes first.
pub(crate) async fn recorded(node: &sim::node::Node, kept: &Kept, accepted: usize) {
    let deadline = node.clock().now() + Span::SECOND;
    loop {
        let recorded = kept.lock().expect("not poisoned").len();
        if recorded == accepted {
            break;
        }
        let now = node.clock().now();
        assert!(
            now < deadline,
            "{recorded} of {accepted} serve tasks recorded"
        );
        node.clock().sleep(Span::MICROSECOND).await;
    }
}

/// Sets `rules` on `hub` unless `None`, and gives the session of the first program
/// that dials the node, with its link on `hub`.
pub(crate) async fn accept_on(
    hub: &hub::Hub,
    node: &sim::node::Node,
    tasks: &env::tasks::Tasks,
    rules: Option<access::Rules>,
) -> (Session, hub::Link) {
    if let Some(rules) = rules {
        hub.set_rules(rules);
    }
    let transport = transport(node, tasks, &own_pool(), HOME, 1 << 16);
    let session = transport.accept().await.expect("a session");
    let link = hub.link(session.clone());
    (session, link)
}

/// Runs `home` on one simulated node, and `program` on another, with the home's
/// address.
pub(crate) fn run_program<F, H, G, P>(seed: u64, home: F, program: G)
where
    F: FnOnce(sim::node::Node, env::tasks::Tasks) -> H + Send + 'static,
    H: Future<Output = ()> + 'static,
    G: FnOnce(sim::node::Node, env::tasks::Tasks, Address) -> P + Send + 'static,
    P: Future<Output = ()> + 'static,
{
    run_program_on(seed, sim::link::Config::default(), home, program);
}

pub(crate) fn run_program_on<F, H, G, P>(
    seed: u64,
    wire: sim::link::Config,
    home: F,
    program: G,
) where
    F: FnOnce(sim::node::Node, env::tasks::Tasks) -> H + Send + 'static,
    H: Future<Output = ()> + 'static,
    G: FnOnce(sim::node::Node, env::tasks::Tasks, Address) -> P + Send + 'static,
    P: Future<Output = ()> + 'static,
{
    let mut sim = sim::Sim::new(sim::Config {
        seed,
        link: wire,
        ..sim::Config::default()
    });
    let nodes = [1, 2].map(|_| sim.node(sim::node::Config::default()));
    let at = Address::Udp(SocketAddr::new(nodes[0].addresses()[0], PORT));
    let shard = |name: &str| env::shards::Config {
        name: name.into(),
        core: None,
    };
    let node = nodes[0].clone();
    let main = move |tasks: env::tasks::Tasks| home(node, tasks);
    drop(
        nodes[0]
            .shards()
            .start(shard("home"), main)
            .expect("starts"),
    );
    let node = nodes[1].clone();
    let main = move |tasks: env::tasks::Tasks| program(node, tasks, at);
    drop(
        nodes[1]
            .shards()
            .start(shard("program"), main)
            .expect("starts"),
    );
    sim.run().expect("the run ends");
}

/// What `serve` gave for one stream, once it replied to a request with its body
/// reversed after [`HOLD`], or the error of that reply.
pub(crate) async fn answer(
    serve: impl Future<Output = Result<Served, serve::Error>>,
    clock: &env::clock::Clock,
) -> Result<Got, serve::Error> {
    match serve.await? {
        Served::Ended => Ok(Got::Ended),
        Served::Request(request) => {
            let subject = request.admitted.hello.subject.clone();
            let reply: Vec<u8> = request.body.iter().rev().copied().collect();
            clock.sleep(HOLD).await;
            request.reply.send(&reply).await?;
            Ok(Got::Request(subject, request.body))
        }
    }
}

pub(crate) fn closed_with(code: u32) -> transport::Error {
    transport::Error::PeerClosed { code: Code(code) }
}
