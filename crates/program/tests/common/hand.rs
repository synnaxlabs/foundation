//! A node by hand: it admits the hello of a program on a hub, and the test sends each
//! reply of each other stream.

use transport::stream::{Incoming, Sender};
use types::frame::{Path, Range};
use wire::hub::client::{Body, Place, Places, Read, place};
use wire::hub::{Head, Reply, ends};

use program::Session;

use crate::agent::AGENT;
use crate::connect::connect;
use crate::home::accept;
use crate::net::{header, own_pool};
use crate::sessions::{rules, run_program};

/// Runs a program whose session connects to a home that admits its hello on a link,
/// then gives the transport session of the home to `rest`, which serves what follows
/// by hand.
pub(crate) fn by_hand<R>(
    seed: u64,
    rest: impl FnOnce(transport::Session, sim::node::Node) -> R + Send + 'static,
    program: impl FnOnce(
        Session,
        sim::node::Node,
    ) -> std::pin::Pin<Box<dyn Future<Output = ()>>>
    + Send
    + 'static,
) where
    R: Future<Output = ()> + 'static,
{
    let home = move |node: sim::node::Node, tasks: env::tasks::Tasks| async move {
        let (test, session, link) = accept(&node, &tasks, true, Some(rules())).await;
        let mut hello = session.accept().await.expect("a stream");
        header(&mut hello).await;
        let hello = link.serve(hello);
        tasks.spawn(async move {
            drop(hello.await);
        });
        rest(session, node).await;
        drop((link, test));
    };
    run_program(seed, home, |node, tasks, at| async move {
        let session = connect(&node, tasks, at, AGENT).await.expect("connects");
        program(session, node).await;
    });
}

/// Sends `bytes` on `sender` as one message.
pub(crate) async fn send(sender: &mut Sender, bytes: &[u8]) {
    let message = own_pool().copy(bytes).expect("room");
    sender.send(message).await.expect("sends");
}

/// Sends `reply` on `sender`.
pub(crate) async fn reply(sender: &mut Sender, reply: Reply) {
    let mut bytes = vec![0; reply.encoded_len()];
    reply.encode(&mut bytes);
    send(sender, &bytes).await;
}

/// Accepts the next read stream of `session` and takes its read and its open. Gives the
/// stream.
pub(crate) async fn accept_read(session: &transport::Session) -> Incoming {
    accept_stream(session, |first| Read::decode(first).expect("a read").body()).await
}

/// Accepts the next stream of `session` and takes its first message, which `body`
/// decodes to the body of its open, and the open. Gives the stream.
pub(crate) async fn accept_stream(
    session: &transport::Session,
    body: impl FnOnce(&[u8]) -> Body,
) -> Incoming {
    let mut incoming = session.accept().await.expect("a stream");
    header(&mut incoming).await;
    let mut next = async || {
        let message = incoming.receiver.recv().await.expect("receives");
        message.expect("a message before the finish")
    };
    let mut body = body(&next().await);
    while body.remain() > 0 {
        body.take(&next().await).expect("bytes of the open");
    }
    incoming
}

/// Sends the start of a read on `read`: a places of `places`, then the run of the
/// first `sent` of `places`.
pub(crate) async fn start(read: &mut Incoming, places: &[Place], sent: usize) {
    let sender = read.sender.as_mut().expect("two-way");
    let mut bytes = [0; Places::LEN];
    let count = u32::try_from(places.len()).expect("fits");
    Places { count }.encode(&mut bytes);
    send(sender, &bytes).await;
    for message in place::messages(&places[..sent], 1 << 10) {
        let mut bytes = vec![0; message.encoded_len()];
        message.encode(&mut bytes);
        send(sender, &bytes).await;
    }
}

/// Accepts the next read and opens it with `places`.
pub(crate) async fn opened(session: &transport::Session, places: &[Place]) -> Incoming {
    opened_at(session, places, 0).await
}

/// Accepts the next read and opens it with `places` and the open position `live`.
pub(crate) async fn opened_at(
    session: &transport::Session,
    places: &[Place],
    live: u64,
) -> Incoming {
    let mut read = accept_read(session).await;
    start(&mut read, places, places.len()).await;
    reply(
        read.sender.as_mut().expect("two-way"),
        Reply::Opened { live },
    )
    .await;
    read
}

/// Sends a frame on `read`: a head of `count` samples from `seq`, the run of `ends`,
/// and `body` in messages of the lengths of `cuts`.
pub(crate) async fn send_frame(
    read: &mut Incoming,
    range: Range,
    run: &[(u32, u32)],
    body: &[u8],
    cuts: &[usize],
) {
    let sender = read.sender.as_mut().expect("two-way");
    let head = Head {
        path: Path::Live,
        range,
        series: u32::try_from(run.len()).expect("fits"),
    };
    reply(sender, Reply::Head(head)).await;
    let mut bytes = vec![0; run.len() * ends::LEN];
    ends::encode(run.iter().copied(), &mut bytes);
    send(sender, &bytes).await;
    let mut rest = body;
    for &cut in cuts {
        let (message, after) = rest.split_at(cut);
        send(sender, message).await;
        rest = after;
    }
    assert!(rest.is_empty(), "the cuts take the whole body");
}
