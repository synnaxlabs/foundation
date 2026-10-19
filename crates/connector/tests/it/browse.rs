//! Browses through `Supervisor::browse`, with kinds of this file.

use connector::browse::{
    self, Child, Cursor, Device, End, Entry, Facts, Page, Path, Query, Scope, Shape,
    Values,
};
use connector::kind::{Channels, Context, Error, Kind, Table};
use connector::secret::Source;
use connector::testing::{Rig, create_secret};
use document::diagnostic::{Code, Diagnostic};
use document::value::{self, Value};
use document::{Attribute, Document, Map};

/// A kind whose device at `address` holds the folder `Line1` with the point `Temp1`,
/// with the address in its tag. It reads the secret `plant.token`, and its device
/// refuses a token other than `t1`. Its `parse` needs `rate`, which a browse does not.
struct Plant;

impl Kind for Plant {
    type Config = ();
    const SCHEMA: document::schema::Body = connector::testing::READS;

    fn parse(&self, config: &Document) -> Result<(), Vec<Diagnostic>> {
        match config.attributes.get("rate") {
            Some(_) => Ok(()),
            None => Err(vec![Diagnostic::new(
                Code::new("plant.no-rate"),
                None,
                "the block has no rate".into(),
                "Give it a rate".into(),
            )]),
        }
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
        std::future::ready(Ok(()))
    }

    async fn browse(
        &self,
        ctx: &browse::Context<'_>,
        query: &Query,
    ) -> Result<browse::Found, Error> {
        let address = match config(ctx.config(), "address") {
            Some(address) => address.to_owned(),
            None => return Err(Error::Device("no address".into())),
        };
        let source = Source::Secret("plant.token".parse().unwrap());
        if ctx.secret(&source).await?.expose() != b"t1" {
            return Err(Error::Device("the device refused the token".into()));
        }
        browse::walk(&mut Session { address }, query, ctx.clock()).await
    }
}

/// The text of the attribute `key` of `config`.
fn config<'a>(config: &'a Document, key: &str) -> Option<&'a str> {
    match &config.attributes.get(key)?.value.kind {
        value::Kind::String(text) => Some(text),
        _ => None,
    }
}

/// The open session of a [`Plant`] device.
struct Session {
    address: String,
}

impl Device for Session {
    fn level(
        &mut self,
        folder: Option<&Entry>,
        _: Option<&Cursor>,
    ) -> impl Future<Output = Result<Option<Page>, Error>> {
        let child = |name: &str, shape| Child {
            name: name.into(),
            tag: format!("{}/{name}", self.address),
            shape,
        };
        let entries = match folder.map(|folder| folder.tag.as_str()) {
            None => vec![child("Line1", Shape::Folder)],
            Some(_) => vec![child("Temp1", Shape::Point(Facts::default()))],
        };
        std::future::ready(Ok(Some(Page {
            entries,
            next: None,
        })))
    }
}

/// A kind whose device holds one point, named by a number from the browse's random
/// source.
struct Dice;

impl Kind for Dice {
    type Config = ();
    const SCHEMA: document::schema::Body = connector::testing::READS;

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
        std::future::ready(Ok(()))
    }

    async fn browse(
        &self,
        ctx: &browse::Context<'_>,
        query: &Query,
    ) -> Result<browse::Found, Error> {
        let name = ctx.rng().next_u64().to_string();
        browse::walk(&mut Roll { name }, query, ctx.clock()).await
    }
}

/// The open session of a [`Dice`] device.
struct Roll {
    name: String,
}

impl Device for Roll {
    fn level(
        &mut self,
        _: Option<&Entry>,
        _: Option<&Cursor>,
    ) -> impl Future<Output = Result<Option<Page>, Error>> {
        let child = Child {
            name: self.name.clone(),
            tag: self.name.clone(),
            shape: Shape::Point(Facts::default()),
        };
        std::future::ready(Ok(Some(Page {
            entries: vec![child],
            next: None,
        })))
    }
}

/// A kind whose browse reads the secret of its name and gives what the read gives.
struct Vault(&'static str);

impl Kind for Vault {
    type Config = ();
    const SCHEMA: document::schema::Body = connector::testing::READS;

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
        std::future::ready(Ok(()))
    }

    async fn browse(
        &self,
        ctx: &browse::Context<'_>,
        _: &Query,
    ) -> Result<browse::Found, Error> {
        let source = Source::Secret(self.0.parse().expect("a secret name"));
        ctx.secret(&source).await?;
        Err(Error::Device("the secret was read".into()))
    }
}

/// A kind with no browse.
struct Blind;

impl Kind for Blind {
    type Config = ();
    const SCHEMA: document::schema::Body = connector::testing::READS;

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
        std::future::ready(Ok(()))
    }
}

/// A connector block with only `kind`, `node`, and `address`.
fn block(kind: &str) -> Document {
    let text = |key: &str, text: &str| Attribute {
        key: key.into(),
        key_span: None,
        value: Value {
            kind: value::Kind::String(text.into()),
            span: None,
        },
    };
    let attributes = vec![
        text("kind", kind),
        text("node", "n1"),
        text("address", "opc.tcp://plc:4840"),
    ];
    Document {
        attributes: Map::new(attributes).expect("distinct keys"),
        blocks: Vec::new(),
    }
}

/// What `Supervisor::browse` gives for `kind` and `query` over [`block`], with the
/// secret `plant.token` set to `token`, once the run ends.
fn browse(
    kind: &str,
    query: &Query,
    token: &[u8],
) -> Result<Result<browse::Found, Error>, sim::Error> {
    let mut found = browse_each(&[kind], query, token)?;
    Ok(found.remove(0))
}

/// What `Supervisor::browse` gives for each of `kinds` in turn, on one supervisor,
/// as [`browse`] gives one.
fn browse_each(
    kinds: &[&str],
    query: &Query,
    token: &[u8],
) -> Result<Vec<Result<browse::Found, Error>>, sim::Error> {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let kinds: Vec<String> = kinds.iter().map(|&kind| kind.to_owned()).collect();
    let (query, token) = (query.clone(), token.to_vec());
    sim.run_on(&node, move |node, tasks| async move {
        let env = hub::testing::Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks,
        };
        let table = (Table::new().with("plant", Plant))
            .with("dice", Dice)
            .with("missing", Vault("plant.missing"))
            .with("upper", Vault("plant.Token"))
            .with("blind", Blind);
        let rig = Rig::open(env, node.net(), table).await;
        create_secret(&node.files(), &"plant.token".parse().unwrap(), &token).await;
        let mut found = Vec::new();
        for kind in &kinds {
            found.push(rig.supervisor().browse(kind, &block(kind), &query).await);
        }
        found
    })
}

fn query(scope: Scope) -> Query {
    Query {
        start: Path::root(),
        scope,
        values: Values::Skipped,
    }
}

/// The one diagnostic of `result`, which is [`Error::Config`].
fn diagnostic(result: Result<browse::Found, Error>) -> Diagnostic {
    match result {
        Err(Error::Config(mut diagnostics)) if diagnostics.len() == 1 => {
            diagnostics.remove(0)
        }
        other => panic!("not one config diagnostic: {other:?}"),
    }
}

#[test]
fn browses_a_block_with_only_its_kind_node_and_address() {
    let pattern = "**".parse().expect("a pattern");
    let found = browse("plant", &query(Scope::Match(pattern)), b"t1")
        .expect("the run ends")
        .expect("found");
    let entries: Vec<_> = (found.entries.iter())
        .map(|entry| (entry.path.to_string(), entry.tag.as_str()))
        .collect();
    assert_eq!(
        entries,
        [("Line1/Temp1".to_owned(), "opc.tcp://plc:4840/Temp1")]
    );
    assert_eq!(found.end, End::Done);
}

#[test]
fn gives_each_browse_a_new_random_source() {
    let found = browse_each(&["dice", "dice"], &query(Scope::Level(None)), b"t1")
        .expect("the run ends");
    let names: Vec<_> = (found.into_iter())
        .map(|found| found.expect("found").entries[0].path.to_string())
        .collect();
    assert_ne!(names[0], names[1]);
}

#[test]
fn gives_the_error_of_the_kind() {
    let result =
        browse("plant", &query(Scope::Level(None)), b"t2").expect("the run ends");
    let Err(Error::Device(error)) = result else {
        panic!("not a device error: {result:?}");
    };
    assert_eq!(error.to_string(), "the device refused the token");
}

#[test]
fn gives_a_secret_that_is_not_there_as_a_retry() {
    let result =
        browse("missing", &query(Scope::Level(None)), b"t1").expect("the run ends");
    let Err(Error::Retry(error)) = result else {
        panic!("not a retry: {result:?}");
    };
    let expected = concat!(
        r#"the secret secret("plant.missing"): the file "#,
        "/data/secrets/plant.missing is not there. Write the value to it, unreadable ",
        "by other users: chmod o-r ",
        "/data/secrets/plant.missing",
    );
    assert_eq!(error.to_string(), expected);
}

#[test]
fn refuses_a_secret_name_that_the_store_cannot_hold() {
    let result =
        browse("upper", &query(Scope::Level(None)), b"t1").expect("the run ends");
    let expected = Diagnostic::new(
        Code::new("connector.bad-secret-name"),
        None,
        r#"the secret secret("plant.Token") has a name that the store cannot hold"#
            .into(),
        "Rename the secret in lowercase letters, since the file store holds no name \
         with a capital letter"
            .into(),
    );
    assert_eq!(diagnostic(result), expected);
}

#[test]
fn refuses_a_kind_that_cannot_browse() {
    let result =
        browse("blind", &query(Scope::Level(None)), b"t1").expect("the run ends");
    let expected = Diagnostic::new(
        Code::new("connector.no-browse"),
        None,
        "this connector kind cannot browse its device".into(),
        "Write the blocks of the points by hand".into(),
    );
    assert_eq!(diagnostic(result), expected);
}

#[test]
fn refuses_a_kind_that_the_build_does_not_have() {
    let result =
        browse("modbus", &query(Scope::Level(None)), b"t1").expect("the run ends");
    let expected = Diagnostic::new(
        Code::new("connector.unknown-kind"),
        None,
        r#"this build has no connector kind "modbus""#.into(),
        r#"Use one of ["blind", "dice", "missing", "plant", "upper"]"#.into(),
    );
    assert_eq!(diagnostic(result), expected);
}
