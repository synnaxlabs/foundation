//! How a program on the node's host reaches the node that runs on a data directory.

use std::net::SocketAddr;
use std::rc::Rc;

use env::files::Files;
use wire::hub::client::Refusal;

/// The budget of the pool of the transport and the `program::Session`. Its largest
/// block holds a datagram, and the session sends a body in chunks of its largest block.
const POOL: block::Config = match block::Config::new(1 << 21) {
    Ok(config) => config,
    Err(_) => panic!("invariant: the budget fits"),
};

/// The budget of the pool of the drafts of the session's writers and of the frames of
/// its reads. No command writes or reads yet, so it is small. Its largest block is the
/// largest frame that a write or a read takes: a larger one gives
/// `program::Error::Pool`.
const FRAMES: block::Config = match block::Config::new(64 << 10) {
    Ok(config) => config,
    Err(_) => panic!("invariant: the budget fits"),
};

/// What [`reach`] dials with: the files of the data directory, and the seams of the
/// program's transport and of its `program::Session`.
#[derive(Debug)]
pub struct Config {
    /// The files of the data directory, which `reach` only reads.
    pub files: Files,
    /// The network where the program's transport binds its socket.
    pub net: env::net::Net,
    /// The monotonic clock of the transport and of the renewal of the hello.
    pub clock: env::clock::Clock,
    /// The source of the random values of the transport and of the connection key.
    pub entropy: env::entropy::Entropy,
    /// Where the transport and the `program::Session` spawn their tasks.
    pub tasks: env::tasks::Tasks,
}

/// A `program::Session` with the node that runs on the data directory of `config`, as
/// its first admin. It reads `address` and `admin.key`, and makes and locks nothing. It
/// dials the loopback IP of the family in place of an unspecified IP in `address`.
///
/// # Errors
///
/// Each [`Error`], by its doc.
pub async fn reach(config: Config) -> Result<program::Session, Error> {
    let Config {
        files,
        net,
        clock,
        entropy,
        tasks,
    } = config;
    let local = sector::local::read(&files).await.map_err(Error::Local)?;
    let admin = local.admin.ok_or(Error::Key)?;
    let pool = Rc::new(block::Pool::heap(POOL));
    let transport = transport::Client::new(transport::client::Config {
        net,
        clock: clock.clone(),
        entropy: entropy.clone(),
        tasks: tasks.clone(),
        pool: Rc::clone(&pool),
    })
    .map_err(Error::Transport)?;
    let at = local.address;
    let frames = Rc::new(block::Pool::heap(FRAMES));
    let config = program::Config {
        via: local.key,
        node: local.public_key,
        addresses: vec![transport::Address::Udp(at)],
        subject: spec::founding::admin(),
        key: admin,
        clock,
        entropy,
        tasks,
        pool,
        frames: Rc::clone(&frames),
        reads: frames,
    };
    program::Session::connect(&transport, config)
        .await
        .map_err(|error| match error {
            program::Error::Refused(refusal) => Error::Refused(refusal),
            program::Error::Transport(error @ transport::Error::Network { .. }) => {
                Error::Transport(error)
            }
            program::Error::Transport(error @ transport::Error::Unreachable { .. }) => {
                Error::Unanswered { at, error }
            }
            error => Error::Session { at, error },
        })
}

/// Why [`reach`] gave no session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// The files of the directory name no node to reach, as the error says.
    Local(sector::local::Error),
    /// The directory holds no file `admin.key` that a node wrote.
    Key,
    /// The transport of the program did not start, or its socket broke.
    Transport(transport::Error),
    /// No node answers at `at`, where the file `address` says the node is: the dial
    /// gave no session.
    Unanswered {
        /// The address that the program dialed.
        at: SocketAddr,
        /// Why the dial gave no session.
        error: transport::Error,
    },
    /// The node at `at` took the session, then the session or the hello failed with
    /// `error`.
    Session {
        /// The address that the program dialed.
        at: SocketAddr,
        /// Why `program::Session::connect` gave no session.
        error: program::Error,
    },
    /// The node refused the hello that the key in `admin.key` signs.
    Refused(Refusal),
}

#[cfg(test)]
mod tests {
    use super::*;

    // A heap pool under loom needs a loom model.
    #[test]
    #[cfg(not(loom))]
    fn the_pool_holds_a_datagram() {
        let pool = block::Pool::heap(POOL);
        assert!(pool.largest() >= 1472, "{}", pool.largest());
    }

    #[test]
    #[cfg(not(loom))]
    fn the_frames_pool_holds_a_frame_of_32_kib() {
        let pool = block::Pool::heap(FRAMES);
        assert!(pool.largest() >= 32 << 10, "{}", pool.largest());
    }
}
