//! The home's end of a forward of a member, by hand, on a node that runs no mesh.

use transport::Session;
use transport::stream::Incoming;
use wire::Protocol;
use wire::hub::client::write::{Open, Write};
use wire::hub::client::{Place, Places, Signed, place};
use wire::hub::forward::{Forward, places};

use crate::agent::Stream;
use crate::net::own_pool;

/// The first messages of a forward that the member sent: the forward, the hello, the
/// write, its open, and the run of places.
pub(crate) struct Forwarded {
    pub(crate) stream: Stream,
    pub(crate) forward: Forward,
    pub(crate) signed: Signed,
    pub(crate) write: Write,
    pub(crate) open: Open,
    pub(crate) run: Vec<u32>,
}

impl Forwarded {
    /// The next forward that the member sends on `session`, after its first messages.
    pub(crate) async fn accept(session: &Session) -> Self {
        let incoming = accept_hub(session).await.expect("a stream");
        let sender = incoming.sender.expect("a stream both ways");
        let mut stream = Stream::new(own_pool(), sender, incoming.receiver);
        let mut next = async || {
            let message = stream.recv().await.expect("receives");
            message.expect("a message before the finish")
        };
        let forward = Forward::decode(&next().await).expect("a forward");
        let signed = Signed::decode(&next().await).expect("a hello");
        let write = Write::decode(&next().await).expect("a write");
        let mut rest = write.body();
        let mut body = Vec::new();
        while rest.remain() > 0 {
            body.extend_from_slice(rest.take(&next().await).expect("a body message"));
        }
        let open = Open::decode(&body).expect("an open");
        let channels = u32::try_from(open.channels.len()).expect("a count");
        let mut decoder = places::Run::new(forward, channels);
        let mut run = Vec::new();
        while !decoder.ended() {
            run.extend(decoder.decode(&next().await).expect("a run"));
        }
        Self {
            stream,
            forward,
            signed,
            write,
            open,
            run,
        }
    }
}

/// Sends `places` on `stream`: their count, then the places in messages of at most
/// `most` bytes.
pub(crate) async fn send_places(stream: &mut Stream, places: &[Place], most: usize) {
    let mut out = [0; Places::LEN];
    let count = u32::try_from(places.len()).expect("a count");
    Places { count }.encode(&mut out);
    stream.send(&out).await;
    for message in place::messages(places, most) {
        let mut out = vec![0; message.encoded_len()];
        message.encode(&mut out);
        stream.send(&out).await;
    }
}

/// The next stream of `session` whose header names the hub protocol, with the header
/// read. The mesh of the member sends `raft` messages on the same session, and this
/// home, which runs no mesh, drops each mesh stream.
pub(crate) async fn accept_hub(
    session: &Session,
) -> Result<Incoming, transport::Error> {
    loop {
        let mut incoming = session.accept().await?;
        let header = incoming.receiver.recv().await?;
        let header = header.expect("the header comes before the finish");
        match wire::header::decode(&header) {
            Ok((Protocol::Hub, [])) => return Ok(incoming),
            Ok((Protocol::Mesh, [])) => drop(incoming),
            other => panic!("a stream of the hub or the mesh: {other:?}"),
        }
    }
}
