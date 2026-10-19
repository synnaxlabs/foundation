//! Kinds whose device is a tree in memory, for the tests of `browse` and `import`.

use std::collections::BTreeMap;

use connector::browse::{
    self, Access, Child, Cursor, Device, Entry, Facts, Found, Page, Query, Shape,
};
use connector::kind::{self, Channels, Context};
use document::diagnostic::{Code, Diagnostic, Note};
use document::value::{self, Kind};
use document::{Attribute, Block, Document, Label, Map};
use env::clock::Clock;
use types::name::Name;
use types::sample::{Scalar, Type, Value};
use types::time::{Span, Stamp};

/// The code of the diagnostic of an `address` of `"bad"`.
pub(crate) const ADDRESS: Code = Code::new("tree.address");

/// A kind whose device is a tree in memory. Its cursor is the offset of the next
/// entry. It gives [`ADDRESS`] for a connector with the `address` `"bad"`. It writes
/// the channel of each `read` block. Its import gives `write "<channel>" { tag =
/// "<tag>" }` for a point of `Access::Write`, else a `read` block.
pub(crate) struct Tree {
    /// The entries of each folder, by the tag of the folder. The tag of the root is
    /// empty.
    folders: BTreeMap<String, Vec<Child>>,
    /// The value of each point that has one, by tag.
    values: BTreeMap<String, Value>,
    /// The most entries of a page.
    size: usize,
    /// When `Some`, each call of the device fails with it.
    failure: Option<&'static str>,
    /// The time of each page.
    step: Span,
}

/// The values of the points of `Types`, by name, in device order.
fn typed() -> Vec<(&'static str, Value)> {
    vec![
        ("bool", Value::Bool(true)),
        ("i8", Value::I8(-8)),
        ("i16", Value::I16(-16)),
        ("i32", Value::I32(-32)),
        ("i64", Value::I64(i64::MIN)),
        ("u8", Value::U8(8)),
        ("u16", Value::U16(16)),
        ("u32", Value::U32(32)),
        ("u64", Value::U64(u64::MAX)),
        ("f32", Value::F32(0.1)),
        ("f64", Value::F64(-2.5)),
        ("nan", Value::F64(f64::NAN)),
        ("inf", Value::F32(f32::INFINITY)),
        ("minus", Value::F64(f64::NEG_INFINITY)),
        (
            "stamp",
            Value::Stamp(Stamp::from_nanos(1_700_000_000_000_000_001)),
        ),
        ("span", Value::Span(Span::from_nanos(250_000))),
        (
            "uuid",
            Value::Uuid(
                *b"\x01\x23\x45\x67\x89\xab\xcd\xef\x01\x23\x45\x67\x89\xab\xcd\xef",
            ),
        ),
    ]
}

/// A point of `facts` with the name `name` and the tag `tag`.
fn point(name: &str, tag: &str, facts: Facts) -> Child {
    Child {
        name: name.to_owned(),
        tag: tag.to_owned(),
        shape: Shape::Point(facts),
    }
}

/// A folder with the name `name` and the tag `tag`.
fn folder(name: &str, tag: &str) -> Child {
    Child {
        name: name.to_owned(),
        tag: tag.to_owned(),
        shape: Shape::Folder,
    }
}

/// `count` points of `f64` under the folder `tag`, whose names are `p<n>`.
fn points(tag: &str, count: usize) -> Vec<Child> {
    let facts = Facts {
        data_type: Some(Type::Scalar(Scalar::F64)),
        ..Facts::default()
    };
    (0..count)
        .map(|n| point(&format!("p{n}"), &format!("{tag}.p{n}"), facts.clone()))
        .collect()
}

/// The entries of `Line3`: a folder, points with each fact, with no fact, and with a
/// unit code and no unit, and an unreadable point.
fn line3() -> Vec<Child> {
    vec![
        folder("Cell", "line3.cell"),
        point(
            "Temperature",
            "line3.temperature",
            Facts {
                data_type: Some(Type::Scalar(Scalar::F64)),
                unit: Some("°C".to_owned()),
                unit_code: Some("CEL".to_owned()),
                access: Some(Access::Read),
            },
        ),
        point("Bare", "line3.bare", Facts::default()),
        point(
            "Heater",
            "line3.heater",
            Facts {
                data_type: Some(Type::Scalar(Scalar::U16)),
                unit: None,
                unit_code: Some("KWT".to_owned()),
                access: Some(Access::Write),
            },
        ),
        Child {
            name: "Recipe".to_owned(),
            tag: "line3.recipe".to_owned(),
            shape: Shape::Unreadable("an array".to_owned()),
        },
        point(
            "Setpoint",
            "line3.setpoint",
            Facts {
                data_type: Some(Type::Scalar(Scalar::F32)),
                unit: Some("%".to_owned()),
                unit_code: Some("P1".to_owned()),
                access: Some(Access::ReadWrite),
            },
        ),
    ]
}

impl Tree {
    /// The tree of the kind `tree`, with pages of 4 entries. The root holds the
    /// folders `Line3` (of [`line3`], with the unreadable point `Cell/Matrix`),
    /// `Types`, `Many`, and `Huge`. `Types` holds a point of each value of [`typed`],
    /// then a point with no value. `Many` holds 501 points and `Huge` 10,001.
    pub(crate) fn create() -> Self {
        let mut folders = BTreeMap::new();
        folders.insert(
            String::new(),
            vec![
                folder("Line3", "line3"),
                folder("Types", "types"),
                folder("Many", "many"),
                folder("Huge", "huge"),
            ],
        );
        folders.insert("line3".to_owned(), line3());
        let matrix = Child {
            name: "Matrix".to_owned(),
            tag: "line3.cell.matrix".to_owned(),
            shape: Shape::Unreadable("a matrix".to_owned()),
        };
        folders.insert("line3.cell".to_owned(), vec![matrix]);
        let mut values = BTreeMap::from([
            ("line3.temperature".to_owned(), Value::F64(21.5)),
            ("line3.heater".to_owned(), Value::U16(3)),
            ("line3.setpoint".to_owned(), Value::F32(42.0)),
        ]);
        let mut types = Vec::new();
        for (name, value) in typed() {
            let tag = format!("types.{name}");
            let facts = Facts {
                data_type: Some(Type::Scalar(value.scalar())),
                access: Some(Access::Read),
                ..Facts::default()
            };
            types.push(point(name, &tag, facts));
            values.insert(tag, value);
        }
        types.push(point("none", "types.none", Facts::default()));
        folders.insert("types".to_owned(), types);
        folders.insert("many".to_owned(), points("many", 501));
        folders.insert("huge".to_owned(), points("huge", 10_001));
        Self {
            folders,
            values,
            size: 4,
            failure: None,
            step: Span::ZERO,
        }
    }

    /// The tree of the kind `broken`: one page of `Line3` holds 501 entries.
    pub(crate) fn create_broken() -> Self {
        Self {
            folders: BTreeMap::from([
                (String::new(), vec![folder("Line3", "line3")]),
                ("line3".to_owned(), points("line3", 501)),
            ]),
            values: BTreeMap::new(),
            size: 501,
            failure: None,
            step: Span::ZERO,
        }
    }

    /// The tree of the kind `failing`: each call of the device fails with
    /// `the server closed the session`.
    pub(crate) fn create_failing() -> Self {
        Self {
            failure: Some("the server closed the session"),
            ..Self::create()
        }
    }

    /// The tree of the kind `slow`: the tree of `tree`, with a second for each page.
    pub(crate) fn create_slow() -> Self {
        Self {
            step: Span::SECOND,
            ..Self::create()
        }
    }

    /// The failure of a call of the device, if the tree has one.
    fn failed(&self) -> Result<(), kind::Error> {
        match self.failure {
            Some(failure) => Err(kind::Error::Device(failure.into())),
            None => Ok(()),
        }
    }

    /// The page of `folder` from `cursor`.
    fn page(
        &self,
        folder: Option<&Entry>,
        cursor: Option<&Cursor>,
    ) -> Result<Option<Page>, kind::Error> {
        self.failed()?;
        let tag = folder.map_or("", |folder| folder.tag.as_str());
        let Some(children) = self.folders.get(tag) else {
            return Ok(None);
        };
        let from =
            cursor.map_or(0, |cursor| cursor.as_str().parse().expect("an offset"));
        let to = children.len().min(from + self.size);
        let next = (to < children.len()).then(|| Cursor::new(to.to_string()));
        Ok(Some(Page {
            entries: children[from..to].to_vec(),
            next,
        }))
    }

    /// The value of each of `tags`.
    fn read(&self, tags: &[&str]) -> Result<Vec<Option<Value>>, kind::Error> {
        self.failed()?;
        Ok(tags
            .iter()
            .map(|tag| self.values.get(*tag).copied())
            .collect())
    }
}

/// The diagnostic of `config` when its `address` is `"bad"`.
fn address(config: &Document) -> Option<Diagnostic> {
    let attribute = config.attributes.get("address")?;
    let Kind::String(text) = &attribute.value.kind else {
        return None;
    };
    if &**text != "bad" {
        return None;
    }
    let mut diagnostic = Diagnostic::new(
        ADDRESS,
        attribute.value.span,
        "the address \"bad\" names no device".to_owned(),
        "Give the address of a device".to_owned(),
    );
    diagnostic.notes.push(Note {
        span: attribute.key_span?,
        text: "the key of the address".to_owned(),
    });
    Some(diagnostic)
}

/// The block `<keyword> "<channel>" { tag = "<tag>" }`.
fn block(keyword: &str, tag: &str, channel: &Name) -> Block {
    let tag = Attribute {
        key: "tag".into(),
        key_span: None,
        value: value::Value {
            kind: Kind::String(tag.into()),
            span: None,
        },
    };
    Block {
        keyword: keyword.into(),
        keyword_span: None,
        labels: vec![Label {
            text: channel.as_str().into(),
            span: None,
        }],
        body: Document {
            attributes: Map::new(vec![tag]).expect("invariant: one key"),
            blocks: Vec::new(),
        },
        span: None,
    }
}

/// One browse of a [`Tree`], on the clock of the browse.
struct Session<'a> {
    tree: &'a Tree,
    clock: &'a Clock,
}

impl Device for Session<'_> {
    async fn level(
        &mut self,
        folder: Option<&Entry>,
        cursor: Option<&Cursor>,
    ) -> Result<Option<Page>, kind::Error> {
        self.clock.sleep(self.tree.step).await;
        self.tree.page(folder, cursor)
    }

    fn values(
        &mut self,
        tags: &[&str],
    ) -> impl Future<Output = Result<Vec<Option<Value>>, kind::Error>> {
        std::future::ready(self.tree.read(tags))
    }
}

impl kind::Kind for Tree {
    type Config = Vec<Name>;
    const SCHEMA: document::schema::Body = connector::testing::READS;

    fn parse(&self, config: &Document) -> Result<Vec<Name>, Vec<Diagnostic>> {
        let reads = config
            .blocks
            .iter()
            .filter(|block| &*block.keyword == "read");
        Ok(reads
            .map(|block| block.labels[0].text.parse().expect("a channel name"))
            .collect())
    }

    fn check(&self, reads: &Vec<Name>) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels {
            writes: reads.clone(),
            ..Channels::default()
        })
    }

    fn run(
        &self,
        _: Context<Vec<Name>>,
    ) -> impl Future<Output = Result<(), kind::Error>> {
        std::future::ready(Ok(()))
    }

    fn block(
        &self,
        tag: &str,
        facts: &Facts,
        channel: &Name,
    ) -> Result<Vec<Block>, Vec<Diagnostic>> {
        let keyword = match facts.access {
            Some(Access::Write) => "write",
            _ => "read",
        };
        Ok(vec![block(keyword, tag, channel)])
    }

    fn tag(&self, block: &Block) -> Option<String> {
        if !matches!(&*block.keyword, "read" | "write") {
            return None;
        }
        match &block.body.attributes.get("tag")?.value.kind {
            Kind::String(tag) => Some(tag.to_string()),
            _ => None,
        }
    }

    async fn browse(
        &self,
        ctx: &browse::Context<'_>,
        query: &Query,
    ) -> Result<Found, kind::Error> {
        if let Some(diagnostic) = address(ctx.config()) {
            return Err(kind::Error::Config(vec![diagnostic]));
        }
        let mut session = Session {
            tree: self,
            clock: ctx.clock(),
        };
        browse::walk(&mut session, query, ctx.clock()).await
    }
}
