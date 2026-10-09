//! The inputs of a supervisor and the status channels of a connector, for the tests
//! of `connector` and of the kinds.

use std::sync::Arc;

use env::net::Net;
use spec::channel::{Channel, Data, Kind};
use spec::data_type::DataType;
use spec::definition::Definition;
use types::channel;
use types::name::Name;

use crate::kind::Table;
use crate::{status, supervisor};

/// A supervisor's inputs for the tests of `connector` and the kinds: `kinds`, the
/// seams of `env` and `net`, and a hub on a new shard in `env`.
///
/// # Panics
///
/// As [`hub::testing::open`].
pub async fn create_config(
    env: hub::testing::Env,
    net: Net,
    kinds: Table,
) -> supervisor::Config {
    let (clock, entropy, tasks) =
        (env.clock.clone(), env.entropy.clone(), env.tasks.clone());
    let (hub, _) = hub::testing::open(env).await;
    supervisor::Config {
        kinds: Arc::new(kinds),
        clock,
        entropy,
        net,
        tasks,
        hub,
    }
}

/// The definitions of the status channels of `connector`, whose kind names `counts`,
/// with keys from `first` on, the index first. A test passes them to
/// [`hub::Hub::set_definitions`] with its own.
///
/// # Panics
///
/// As [`status::channels`].
#[must_use]
pub fn create_status(
    connector: &Name,
    counts: &[Name],
    first: channel::Key,
) -> Vec<(Name, Definition)> {
    let (time, channels) = status::channels(connector, counts);
    let index = first;
    let kind = Kind::Index {
        error: None,
        control: None,
    };
    let time = (time, Definition::Channel(Channel { key: index, kind }));
    let channels =
        (first.as_u128() + 1..)
            .zip(channels)
            .map(|(key, (name, sample))| {
                let data = Data::new(index, None, DataType::Sample(sample), None);
                let data = data.expect("invariant: a status channel has no unit");
                let key = channel::Key::from_u128(key);
                let kind = Kind::Data(data);
                (name, Definition::Channel(Channel { key, kind }))
            });
    std::iter::once(time).chain(channels).collect()
}
