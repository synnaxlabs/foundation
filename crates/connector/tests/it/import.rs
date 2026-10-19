//! Gives the blocks of a point through `Table::block`, and the tag of a block through
//! `Table::tag`, with kinds of this file.

use connector::browse::{Access, Facts};
use connector::kind::{Channels, Context, Error, Kind, Table};
use document::diagnostic::{Code, Diagnostic};
use document::value::{self, Value};
use document::{Attribute, Block, Document, Label, Map};
use types::name::Name;

/// A kind whose block of a point is `read "<channel>" { tag = "<tag>" }`, or a `write`
/// block in its place for a point that a connector can only write. The tag of a block
/// is its `tag`.
struct Points;

impl Kind for Points {
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

    fn block(
        &self,
        tag: &str,
        facts: &Facts,
        channel: &Name,
    ) -> Result<Vec<Block>, Vec<Diagnostic>> {
        let keyword = match facts.access {
            Some(Access::Write) => "write",
            None | Some(Access::Read | Access::ReadWrite) => "read",
        };
        Ok(vec![point(keyword, channel, tag)])
    }

    fn tag(&self, block: &Block) -> Option<String> {
        match &block.body.attributes.get("tag")?.value.kind {
            value::Kind::String(tag) => Some(tag.to_string()),
            _ => None,
        }
    }
}

/// A kind with no blocks.
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

/// `<keyword> "<channel>" { tag = "<tag>" }`.
fn point(keyword: &str, channel: &Name, tag: &str) -> Block {
    let tag = Attribute {
        key: "tag".into(),
        key_span: None,
        value: Value {
            kind: value::Kind::String(tag.into()),
            span: None,
        },
    };
    let label = Label {
        text: channel.as_str().into(),
        span: None,
    };
    Block {
        keyword: keyword.into(),
        keyword_span: None,
        labels: vec![label],
        body: Document {
            attributes: Map::new(vec![tag]).expect("one key"),
            blocks: Vec::new(),
        },
        span: None,
    }
}

fn table() -> Table {
    Table::new().with("blind", Blind).with("points", Points)
}

fn channel() -> Name {
    "plc.Line1.Temp".parse().expect("a valid name")
}

fn facts(access: Option<Access>) -> Facts {
    Facts {
        access,
        ..Facts::default()
    }
}

#[test]
fn gives_the_blocks_of_the_kind() {
    let blocks = table().block("points", "ns=2;s=T1", &facts(None), &channel());
    assert_eq!(blocks, Ok(vec![point("read", &channel(), "ns=2;s=T1")]));
}

#[test]
fn gives_the_write_block_of_a_point_that_a_connector_can_only_write() {
    let access = Some(Access::Write);
    let blocks = table().block("points", "ns=2;s=T1", &facts(access), &channel());
    assert_eq!(blocks, Ok(vec![point("write", &channel(), "ns=2;s=T1")]));
}

#[test]
fn refuses_a_kind_that_cannot_write_a_block() {
    let result = table().block("blind", "ns=2;s=T1", &Facts::default(), &channel());
    let expected = Diagnostic::new(
        Code::new("connector.no-import"),
        None,
        "this connector kind cannot write the block of a point".into(),
        "Write the blocks of the points by hand".into(),
    );
    assert_eq!(result, Err(vec![expected]));
}

#[test]
fn refuses_a_kind_that_the_build_does_not_have() {
    let result = table().block("modbus", "ns=2;s=T1", &Facts::default(), &channel());
    let expected = Diagnostic::new(
        Code::new("connector.unknown-kind"),
        None,
        r#"this build has no connector kind "modbus""#.into(),
        r#"Use one of ["blind", "points"]"#.into(),
    );
    assert_eq!(result, Err(vec![expected]));
}

#[test]
fn gives_the_tag_of_each_block_that_the_kind_gives() {
    for access in [None, Some(Access::Write)] {
        let blocks = table().block("points", "ns=2;s=T1", &facts(access), &channel());
        for block in blocks.expect("blocks") {
            let tag = table().tag("points", &block);
            assert_eq!(tag, Ok(Some("ns=2;s=T1".into())), "{access:?}");
        }
    }
}

#[test]
fn gives_no_tag_from_a_kind_with_no_blocks() {
    let block = point("read", &channel(), "ns=2;s=T1");
    assert_eq!(table().tag("blind", &block), Ok(None));
}

#[test]
fn refuses_the_tag_of_a_kind_that_the_build_does_not_have() {
    let block = point("read", &channel(), "ns=2;s=T1");
    let expected = Diagnostic::new(
        Code::new("connector.unknown-kind"),
        None,
        r#"this build has no connector kind "modbus""#.into(),
        r#"Use one of ["blind", "points"]"#.into(),
    );
    assert_eq!(table().tag("modbus", &block), Err(vec![expected]));
}
