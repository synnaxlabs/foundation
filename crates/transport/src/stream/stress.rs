//! Random runs of many streams over lossy links, with cancelled sends and reads.

use std::collections::BTreeMap;
use std::future::{pending, poll_fn};
use std::num::{NonZeroU64, NonZeroUsize};
use std::pin::pin;
use std::rc::Rc;
use std::slice;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use block::{Block, Heap, Pool};
use env::clock::Clock;
use sim::node::Node;
use types::time::Span;

use crate::testing::{self, spans};
use crate::{Address, Class, Code, Config, Error, Transport};

const CANCELLED: Error = Error::Reset { code: Code(0) };
const REPLY: u64 = 1 << 40;

fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = mix(self.0);
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }

    fn chance(&mut self, permille: u64) -> bool {
        self.below(1000) < permille
    }
}

#[derive(Clone, Copy, Debug)]
struct Plan {
    case: u64,
    bytes_max: usize,
    window: usize,
    streams: u32,
    replies: u32,
    scarce: bool,
    cancel: u64,
    recv_timeout: u64,
    delay_ms: u64,
    jitter_ms: u64,
    loss: u64,
    duplication: u64,
    rate: Option<u64>,
}

fn scarce_pool() -> Pool {
    let config = block::Config { budget: 1 << 16 };
    let memory = Heap::new(config.reservation());
    Pool::new(config, memory)
}

fn send_pool() -> Pool {
    let config = block::Config { budget: 1 << 20 };
    let memory = Heap::new(config.reservation());
    Pool::new(config, memory)
}

impl Plan {
    fn new(case: u64) -> Self {
        let mut rng = Rng(mix(case));
        let scarce = rng.chance(300);
        let sizes = [1472, 4096, 16384, 65536];
        let mut bytes_max = sizes[usize::try_from(rng.below(4)).expect("small")];
        if scarce {
            bytes_max = bytes_max.min(scarce_pool().largest());
        }
        let window = bytes_max * usize::try_from(1 + rng.below(4)).expect("small")
            + usize::try_from(rng.below(4096)).expect("small");
        Self {
            case,
            bytes_max,
            window,
            streams: u32::try_from(1 + rng.below(8)).expect("small"),
            replies: u32::try_from(rng.below(8)).expect("small"),
            scarce,
            cancel: [0, 20, 100][usize::try_from(rng.below(3)).expect("small")],
            recv_timeout: [0, 50, 200][usize::try_from(rng.below(3)).expect("small")],
            delay_ms: 1 + rng.below(20),
            jitter_ms: rng.below(20),
            loss: rng.below(150),
            duplication: rng.below(50),
            rate: rng.chance(300).then(|| 100_000 + rng.below(10_000_000)),
        }
    }

    /// The class, whether it goes both ways, and the message count of stream `s`.
    fn stream(&self, s: u32) -> (Class, bool, u32) {
        let mut rng = Rng(mix(self.case ^ 0xabc0_0000 ^ u64::from(s)));
        let classes = [
            Class::Command,
            Class::Latest,
            Class::Complete,
            Class::CatchUp,
        ];
        let class = classes[usize::try_from(rng.below(4)).expect("small")];
        let bi = rng.chance(500);
        let count = u32::try_from(rng.below(21)).expect("small");
        (class, bi, count)
    }

    fn link(&self) -> sim::link::Config {
        let ms = |n: u64| spans(Span::MILLISECOND, i64::try_from(n).expect("small"));
        sim::link::Config {
            delay: ms(self.delay_ms),
            jitter: ms(self.jitter_ms),
            loss: permille(self.loss),
            duplication: permille(self.duplication),
            rate: self.rate.and_then(NonZeroU64::new),
            ..sim::link::Config::default()
        }
    }
}

fn permille(n: u64) -> f64 {
    f64::from(u32::try_from(n).expect("small")) / 1000.0
}

fn len(case: u64, stream: u64, index: u64, bytes_max: usize) -> usize {
    let h = mix(case ^ mix(stream ^ mix(index)));
    let max = bytes_max as u64;
    let len = match h % 8 {
        0 => 0,
        1 => max,
        2 => max - 1,
        3 => 1,
        4 | 5 => (h >> 8) % 64,
        _ => (h >> 8) % (max + 1),
    };
    usize::try_from(len).expect("small")
}

fn body(case: u64, stream: u64, index: u64, len: usize) -> Vec<u8> {
    let base =
        usize::try_from(mix(case ^ (stream << 20) ^ index) >> 40).expect("small");
    (0..len)
        .map(|j| u8::try_from((base + j * 7 + (j >> 8)) % 256).expect("a byte"))
        .collect()
}

async fn block(pool: &Pool, clock: &Clock, bytes: &[u8]) -> Block {
    loop {
        if let Ok(mut unique) = pool.alloc(bytes.len()) {
            unique.copy_from_slice(bytes);
            return unique.freeze();
        }
        clock.sleep(Span::MILLISECOND).await;
    }
}

/// `work`'s output, or `None` when `span` passes first; `work` then drops.
async fn within<F: Future>(clock: &Clock, span: Span, work: F) -> Option<F::Output> {
    let mut work = pin!(work);
    let mut sleep = pin!(clock.sleep(span));
    poll_fn(|cx| {
        if let Poll::Ready(out) = work.as_mut().poll(cx) {
            return Poll::Ready(Some(out));
        }
        sleep.as_mut().poll(cx).map(|()| None)
    })
    .await
}

#[derive(Clone, Debug, PartialEq)]
enum Sent {
    Finished(u32),
    Cancelled(u32),
    Failed(u32, Error),
}

#[derive(Debug, Default)]
struct Report {
    sent: BTreeMap<u32, Sent>,
    replies: BTreeMap<u32, Result<u32, (u32, Error)>>,
    /// Data messages each server stream got, and how it ended.
    received: BTreeMap<u32, (u32, Option<Result<(), Error>>)>,
    anonymous: Vec<Error>,
    accept_end: Option<Error>,
    reply_sent: BTreeMap<u64, Result<(), Error>>,
}

type Shared = Arc<Mutex<Report>>;

async fn send_stream(
    plan: Plan,
    s: u32,
    count: u32,
    mut sender: super::Sender,
    clock: Clock,
    pool: Rc<Pool>,
    report: Shared,
) {
    let mut rng = Rng(mix(plan.case ^ 0x5e4d_0000 ^ u64::from(s)));
    let mut outcome = Sent::Finished(count);
    for i in 0..=count {
        let bytes = if i == 0 {
            let mut header = s.to_le_bytes().to_vec();
            header.extend_from_slice(&count.to_le_bytes());
            header
        } else {
            let (stream, index) = (u64::from(s), u64::from(i));
            body(
                plan.case,
                stream,
                index,
                len(plan.case, stream, index, plan.bytes_max),
            )
        };
        let message = block(&pool, &clock, &bytes).await;
        let sent = if rng.chance(plan.cancel) {
            let nanos = i64::try_from(rng.below(20_000_000)).expect("small");
            let timed = within(&clock, Span::from_nanos(nanos), sender.send(message));
            let Some(sent) = timed.await else {
                // A dropped send resets the stream only when the stream took the
                // message, so reset it in both cases.
                sender.reset(Code(0));
                let mut report = report.lock().expect("lock");
                report.sent.insert(s, Sent::Cancelled(i));
                return;
            };
            sent
        } else {
            sender.send(message).await
        };
        if let Err(error) = sent {
            outcome = Sent::Failed(i, error);
            break;
        }
    }
    if outcome == Sent::Finished(count)
        && let Err(error) = sender.finish()
    {
        outcome = Sent::Failed(count + 1, error);
    }
    report.lock().expect("lock").sent.insert(s, outcome);
}

async fn read_replies(
    plan: Plan,
    s: u32,
    mut receiver: super::Receiver,
    report: Shared,
) {
    let mut n = 0u32;
    let end = loop {
        match receiver.recv().await {
            Ok(Some(message)) => {
                let index = u64::from(n);
                let want = body(
                    plan.case,
                    REPLY,
                    index,
                    len(plan.case, REPLY, index, plan.bytes_max),
                );
                assert_eq!(message.len(), want.len(), "reply {n} of stream {s}");
                assert!(message[..] == want[..], "reply {n} of stream {s} differs");
                n += 1;
            }
            Ok(None) => break Ok(n),
            Err(error) => break Err((n, error)),
        }
    };
    report.lock().expect("lock").replies.insert(s, end);
}

async fn receive(
    plan: Plan,
    k: u64,
    mut receiver: super::Receiver,
    clock: Clock,
    report: Shared,
) {
    let mut rng = Rng(mix(plan.case ^ 0x7ec0_0000 ^ k));
    let mut stream: Option<(u32, u32)> = None;
    let mut n = 0u32;
    let end = loop {
        let read = if rng.chance(plan.recv_timeout) {
            let nanos = i64::try_from(rng.below(20_000_000)).expect("small");
            match within(&clock, Span::from_nanos(nanos), receiver.recv()).await {
                Some(read) => read,
                None => continue,
            }
        } else {
            receiver.recv().await
        };
        match read {
            Ok(Some(message)) => match stream {
                None => {
                    assert_eq!(message.len(), 8, "a header");
                    let s = u32::from_le_bytes(message[..4].try_into().expect("4"));
                    let count = u32::from_le_bytes(message[4..].try_into().expect("4"));
                    stream = Some((s, count));
                    let mut report = report.lock().expect("lock");
                    let old = report.received.insert(s, (0, None));
                    assert!(old.is_none(), "stream {s} twice");
                }
                Some((s, count)) => {
                    n += 1;
                    assert!(n <= count, "stream {s}: message {n} past {count}");
                    let (id, index) = (u64::from(s), u64::from(n));
                    let want = body(
                        plan.case,
                        id,
                        index,
                        len(plan.case, id, index, plan.bytes_max),
                    );
                    assert_eq!(message.len(), want.len(), "stream {s} message {n}");
                    assert!(message[..] == want[..], "stream {s} message {n} differs");
                    report.lock().expect("lock").received.insert(s, (n, None));
                }
            },
            Ok(None) => break Ok(()),
            Err(error) => break Err(error),
        }
    };
    let mut report = report.lock().expect("lock");
    match stream {
        Some((s, _)) => {
            report.received.insert(s, (n, Some(end)));
        }
        None => report
            .anonymous
            .push(end.expect_err("a stream that finished with no header")),
    }
}

async fn reply(
    plan: Plan,
    k: u64,
    mut sender: super::Sender,
    clock: Clock,
    pool: Rc<Pool>,
    report: Shared,
) {
    let mut result = Ok(());
    for i in 0..u64::from(plan.replies) {
        let bytes = body(
            plan.case,
            REPLY,
            i,
            len(plan.case, REPLY, i, plan.bytes_max),
        );
        let message = block(&pool, &clock, &bytes).await;
        result = sender.send(message).await;
        if result.is_err() {
            break;
        }
    }
    if result.is_ok() {
        result = sender.finish();
    }
    report.lock().expect("lock").reply_sent.insert(k, result);
}

fn run(case: u64) -> Result<(), String> {
    let plan = Plan::new(case);
    let report: Shared = Arc::default();
    let (mut sim, client, server) = testing::nodes(case);
    let at = Address::Udp(testing::address(&server));
    sim.link(&client, &server, plan.link());
    sim.link(&server, &client, plan.link());
    let shared = Arc::clone(&report);
    testing::shard(&server, testing::SERVER, move |config, node| {
        serve(plan, config, node, shared)
    });
    let shared = Arc::clone(&report);
    testing::shard(&client, testing::CLIENT, move |config, node| {
        open(plan, at, config, node, shared)
    });
    let ran = sim.run_for(spans(Span::SECOND, 600));
    let report = report.lock().map_err(|error| error.to_string())?;
    let ran = ran.map_err(|error| format!("the run failed: {error:?}"));
    let checked = ran.and_then(|()| check(&plan, &report));
    checked.map_err(|why| format!("case {case}: {why}\n{plan:?}\n{report:#?}"))
}

/// Accepts one session and each stream on it, and replies on each stream both ways.
async fn serve(plan: Plan, config: Config, node: Node, shared: Shared) {
    let tasks = config.tasks.clone();
    let mut config = Config {
        message_bytes_max: NonZeroUsize::new(plan.bytes_max).expect("not zero"),
        window_bytes: plan.window,
        ..config
    };
    if plan.scarce {
        config.pool = Rc::new(scarce_pool());
    }
    let part = testing::part(&node.net(), testing::address(&node));
    let transport = Transport::new(config, part).expect("a transport");
    let session = transport.accept().await.expect("a session");
    let clock = node.clock();
    let pool = Rc::new(send_pool());
    let mut k = 0;
    loop {
        match session.accept().await {
            Ok(incoming) => {
                let report = Arc::clone(&shared);
                let task = receive(plan, k, incoming.receiver, clock.clone(), report);
                tasks.spawn(task);
                if let Some(sender) = incoming.sender {
                    let report = Arc::clone(&shared);
                    let pool = Rc::clone(&pool);
                    tasks.spawn(reply(plan, k, sender, clock.clone(), pool, report));
                }
                k += 1;
            }
            Err(error) => {
                shared.lock().expect("lock").accept_end = Some(error);
                break;
            }
        }
    }
    pending::<()>().await;
    drop((transport, session));
}

/// Dials the server, and opens and sends each stream of `plan`.
async fn open(plan: Plan, at: Address, config: Config, node: Node, shared: Shared) {
    let tasks = config.tasks.clone();
    let config = Config {
        message_bytes_max: NonZeroUsize::new(plan.bytes_max).expect("not zero"),
        window_bytes: plan.window,
        ..config
    };
    let part = testing::part(&node.net(), testing::address(&node));
    let transport = Transport::new(config, part).expect("a transport");
    let server = crate::tls::public(&testing::SERVER);
    let session = loop {
        match transport.dial(server, slice::from_ref(&at)).await {
            Ok(session) => break session,
            Err(Error::TimedOut) => {}
            Err(error) => panic!("the dial failed: {error:?}"),
        }
    };
    let clock = node.clock();
    let pool = Rc::new(send_pool());
    for s in 0..plan.streams {
        let (class, bi, count) = plan.stream(s);
        let sender = if bi {
            let (sender, receiver) = session.open(class).await.expect("a stream");
            let report = Arc::clone(&shared);
            tasks.spawn(read_replies(plan, s, receiver, report));
            sender
        } else {
            session.open_sender(class).await.expect("a stream")
        };
        let report = Arc::clone(&shared);
        let pool = Rc::clone(&pool);
        let task = send_stream(plan, s, count, sender, clock.clone(), pool, report);
        tasks.spawn(task);
    }
    pending::<()>().await;
    drop((transport, session));
}

/// Whether `report` shows each stream of `plan` delivered once, in order.
fn check(plan: &Plan, report: &Report) -> Result<(), String> {
    let mut cancelled = 0;
    for s in 0..plan.streams {
        let (_, bi, _) = plan.stream(s);
        let Some(sent) = report.sent.get(&s) else {
            return Err(format!("the send of stream {s} never ended"));
        };
        let got = report.received.get(&s);
        match sent {
            Sent::Finished(c) => {
                if got != Some(&(*c, Some(Ok(())))) {
                    return Err(format!("stream {s} finished {c} but got {got:?}"));
                }
            }
            Sent::Cancelled(i) => {
                cancelled += 1;
                if let Some((n, end)) = got
                    && (n > i || end.as_ref() != Some(&Err(CANCELLED)))
                {
                    return Err(format!("stream {s} cancelled at {i} but got {got:?}"));
                }
            }
            Sent::Failed(..) => return Err(format!("stream {s}: {sent:?}")),
        }
        if bi {
            let replies = report.replies.get(&s);
            let whole = replies == Some(&Ok(plan.replies));
            let unseen = matches!(sent, Sent::Cancelled(_))
                && got.is_none()
                && replies == Some(&Err((0, CANCELLED)));
            if !whole && !unseen {
                return Err(format!("stream {s} replies: {replies:?}"));
            }
        }
    }
    if report.anonymous.len() > cancelled
        || report.anonymous.iter().any(|error| *error != CANCELLED)
    {
        return Err("streams that ended before their header".to_owned());
    }
    Ok(())
}

/// One test for each case: a run that failed before #1456.
macro_rules! cases {
    ($($name:ident: $case:literal,)*) => {$(
        #[test]
        fn $name() {
            assert_eq!(run($case), Ok(()));
        }
    )*};
}

cases! {
    case_23: 23,
    case_109: 109,
    case_214: 214,
    case_225: 225,
    case_272: 272,
    case_307: 307,
    case_440: 440,
    case_466: 466,
    case_490: 490,
    case_554: 554,
    case_566: 566,
    case_751: 751,
    case_861: 861,
    case_902: 902,
    case_956: 956,
    case_978: 978,
    case_984: 984,
    case_991: 991,
    case_996: 996,
    case_1010: 1010,
    case_1067: 1067,
    case_1095: 1095,
    case_1112: 1112,
    case_1137: 1137,
    case_1173: 1173,
    case_1181: 1181,
    case_1190: 1190,
    case_1237: 1237,
    case_1447: 1447,
    case_1460: 1460,
    case_1550: 1550,
    case_1706: 1706,
    case_1760: 1760,
    case_1795: 1795,
    case_1861: 1861,
    case_1875: 1875,
    case_1894: 1894,
    case_1897: 1897,
    case_1934: 1934,
    case_1973: 1973,
}
