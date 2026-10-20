//! A kind that opens the `poll::Writer` of one `f64` channel and gives it to a body,
//! for the `stamp` bench and the `alloc` test.

use connector::cancel::Token;
use connector::kind::{Channels, Context, Error, Kind, Table};
use connector::poll::{COUNTS, Writer};
use connector::testing::{READS, Rig};
use document::Document;
use document::diagnostic::Diagnostic;
use env::net::Net;
use spec::channel::{Channel, Data, Kind as ChannelKind};
use spec::data_type::DataType;
use spec::definition::Definition;
use types::channel;
use types::name::Name;
use types::sample::{Scalar, Type};

/// A kind whose run opens the [`Writer`] of [`value`], gives it to its body, and ends.
struct Stamped<F>(F);

impl<F: Fn(&mut Writer) + Send + Sync + 'static> Kind for Stamped<F> {
    type Config = ();
    const SCHEMA: document::schema::Body = READS;

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        let counts = COUNTS.map(|count| count.parse().expect("a valid name"));
        Ok(Channels {
            writes: vec![value()],
            counts: counts.to_vec(),
            ..Channels::default()
        })
    }

    async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
        let writer = Writer::open(&ctx, vec![value()]).await?;
        (self.0)(&mut writer.expect("a live token"));
        Ok(())
    }
}

/// The channel that the kind writes.
pub(crate) fn value() -> Name {
    "plant.stamp.value".parse().expect("a valid name")
}

/// The name of the connector that [`run`] runs.
pub(crate) fn connector() -> Name {
    "plant.stamp".parse().expect("a valid name")
}

/// Opens a rig whose one kind gives its writer to `body`, with an index
/// `plant.stamp.time` and its `f64` channel [`value`].
pub(crate) async fn rig(
    env: hub::testing::Env,
    net: Net,
    body: impl Fn(&mut Writer) + Send + Sync + 'static,
) -> Rig {
    let rig = Rig::open(env, net, Table::new().with("stamped", Stamped(body))).await;
    let index = channel::Key::from_u128(100);
    let kind = ChannelKind::Index {
        error: None,
        control: None,
    };
    let time = Definition::Channel(Channel { key: index, kind });
    let sample = DataType::Sample(Type::Scalar(Scalar::F64));
    let data = Data::new(index, None, sample, None).expect("no unit");
    let data = Definition::Channel(Channel {
        key: channel::Key::from_u128(101),
        kind: ChannelKind::Data(data),
    });
    let name = "plant.stamp.time".parse().expect("a valid name");
    rig.define(vec![(name, time), (value(), data)]);
    rig
}

/// Runs the [`connector`] of the kind of `rig` to its end.
///
/// # Panics
///
/// When the run fails.
pub(crate) async fn run(rig: &Rig) {
    let (token, config) = (Token::new(), Document::default());
    rig.open_connector("stamped", connector(), &config, &token)
        .await
        .run()
        .await
        .expect("the connector ends ok");
}
