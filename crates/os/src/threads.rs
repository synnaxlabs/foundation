//! Dedicated threads, each a shard with no core whose main future is the body.

use env::thread::{Error, Handle};
use env::threads::Body;

use crate::shards;

/// Starts each dedicated thread on its own OS thread.
pub(crate) struct Driver;

impl env::threads::Driver for Driver {
    fn start(&self, name: &str, body: Body) -> Result<Handle, Error> {
        shards::start(name.to_owned(), None, Box::new(|_| body()))
    }
}
