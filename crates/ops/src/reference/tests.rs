use connector::kind::{Channels, Context, Error};
use document::diagnostic::Diagnostic;

use super::*;
use crate::operation;

/// Each body of `block` and of each block inside it.
fn bodies(block: &'static Block) -> Vec<&'static Body> {
    block
        .forms
        .iter()
        .flat_map(|form| {
            std::iter::once(form).chain(form.blocks.iter().flat_map(bodies))
        })
        .collect()
}

#[test]
fn docs_state_each_block_form_attribute_and_example() {
    let docs = operation::docs(&crate::common::create_kinds());
    for block in config::SCHEMA {
        let heading = format!("\n## `{}`\n\n", block.keyword);
        assert!(docs.contains(&heading), "{heading}");
        let label = block.label.expect("each core block has a label");
        assert!(docs.contains(&format!("Label: {label}.\n")), "{label}");
        for body in bodies(block) {
            assert!(docs.contains(&format!("\n{}\n\n", body.summary)));
            for attribute in body.attributes {
                let line = format!(
                    "- `{}`: {}. {} {}\n",
                    attribute.name,
                    attribute.value,
                    presence(attribute.presence),
                    attribute.summary
                );
                assert!(docs.contains(&line), "{line}");
            }
            for example in body.examples {
                let fence = format!("```hcl\n{example}```\n");
                assert!(docs.contains(&fence), "{example}");
            }
        }
    }
}

#[test]
fn docs_give_the_forms_of_a_block_in_order() {
    let docs = operation::docs(&crate::common::create_kinds());
    let channel = docs.find("\n## `channel`\n").expect("a channel section");
    let index = docs[channel..].find("\n### Form 1\n\nAn index channel");
    let data = docs[channel..].find("\n### Form 2\n\nA data channel");
    assert!(index.is_some() && index < data, "{docs}");
}

/// A body with no attributes and the blocks `blocks`.
const fn holding(summary: &'static str, blocks: &'static [Block]) -> Body {
    Body {
        summary,
        attributes: &[],
        blocks,
        examples: &[],
    }
}

const INNER: Block = Block {
    keyword: "inner",
    label: None,
    forms: &[holding("Inner.", &[])],
};

const ONE: Block = Block {
    keyword: "one",
    label: None,
    forms: &[holding("One.", &[INNER])],
};

const TWO: Block = Block {
    keyword: "two",
    label: Some("a name"),
    forms: &[holding("First.", &[INNER]), holding("Second.", &[])],
};

const OUTER: Block = Block {
    keyword: "outer",
    label: None,
    forms: &[holding("Outer.", &[TWO])],
};

#[test]
fn a_block_inside_a_body_gets_the_next_heading() {
    let text = markdown(&[ONE, TWO], &Table::new());
    assert!(
        text.starts_with(
            "\n# Config blocks\n\n## `one`\n\nNo label.\n\nOne.\n\n\n### \
             `inner`\n\nNo label.\n\nInner.\n\n\n## `two`\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("\n### Form 1\n\nFirst.\n\n\n#### `inner`\n"),
        "{text}"
    );
    assert!(text.contains("\n### Form 2\n\nSecond.\n"), "{text}");
    let text = markdown(&[OUTER], &Table::new());
    assert!(
        text.contains("\n#### Form 1\n\nFirst.\n\n\n##### `inner`\n"),
        "{text}"
    );
}

#[test]
fn docs_state_each_presence_in_its_words() {
    let docs = operation::docs(&crate::common::create_kinds());
    for line in [
        "- `kind`: \"index\". Required. Makes the channel an index.\n",
        "- `kind`: \"data\". Default: \"data\". Makes the channel a data channel.\n",
        "- `unit`: a unit, such as \"kPa\". Optional. The unit of a numeric sample.\n",
        "- `disk`: a size above zero, such as \"10GiB\". Required unless the body \
         holds `pool`. The disk budget of each node. Without it, the budget of a less \
         specific policy.\n",
    ] {
        assert!(docs.contains(line), "{line}");
    }
}

/// An attribute `name` that each of `others` can take the place of.
const fn unless(name: &'static str, others: &'static [&'static str]) -> Attribute {
    Attribute {
        name,
        value: "a value",
        presence: Presence::Unless(others),
        summary: "It.",
    }
}

const UNLESS: Block = Block {
    keyword: "unless",
    label: None,
    forms: &[Body {
        summary: "Unless.",
        attributes: &[
            unless("one", &["a"]),
            unless("two", &["a", "b"]),
            unless("three", &["a", "b", "c"]),
        ],
        blocks: &[],
        examples: &[],
    }],
};

#[test]
fn unless_joins_the_names_it_takes() {
    let text = markdown(&[UNLESS], &Table::new());
    for line in [
        "- `one`: a value. Required unless the body holds `a`. It.\n",
        "- `two`: a value. Required unless the body holds `a` and `b`. It.\n",
        "- `three`: a value. Required unless the body holds `a`, `b`, and `c`. It.\n",
    ] {
        assert!(text.contains(line), "{text}");
    }
}

#[test]
fn docs_state_each_secret_source() {
    let docs = operation::docs(&crate::common::create_kinds());
    assert!(docs.contains("\n# Config blocks\n\n## `"), "{docs}");
    let sources = docs.find("\n# Secret sources\n").expect("a secret section");
    assert!(docs[sources..].starts_with(
        "\n# Secret sources\n\nA secret attribute takes one of these calls, which \
         names where its value comes from, never the value.\n\n"
    ));
    let mut after = sources;
    for call in CALLS {
        let line = format!("- `{call}`: {}\n", call.summary());
        let at = docs[after..].find(&line).expect(&line);
        after += at + line.len();
    }
    assert!(docs[sources..].contains(
        "- `env(\"INFLUX_TOKEN\")`: The variable of that name in the environment of \
         the node process, as it was when the process began.\n"
    ));
}

/// A kind whose body holds an example, which no kind of the fixtures has.
struct Stated;

impl connector::kind::Kind for Stated {
    type Config = ();
    const SCHEMA: Body = Body {
        summary: "A kind with an example.",
        attributes: &[unless("one", &["a"])],
        blocks: &[INNER],
        examples: &["one = \"x\"\n\ninner {\n}\n"],
    };

    fn parse(&self, _: &document::Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
        std::future::ready(Ok(()))
    }
}

/// A kind whose one example is empty.
struct Empty;

impl connector::kind::Kind for Empty {
    type Config = ();
    const SCHEMA: Body = Body {
        summary: "A kind with an empty example.",
        attributes: &[],
        blocks: &[],
        examples: &[""],
    };

    fn parse(&self, _: &document::Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
        std::future::ready(Ok(()))
    }
}

#[test]
fn docs_state_each_kind_with_each_attribute_block_and_example() {
    let kinds = Table::new()
        .with("stated", Stated)
        .with("influx", crate::common::INFLUX);
    let docs = operation::docs(&kinds);
    let start = docs.find("\n# Connector kinds\n").expect("a kinds section");
    let end = docs.find("\n# Secret sources\n").expect("a secret section");
    let section = &docs[start..end];
    assert!(section.starts_with(
        "\n# Connector kinds\n\nA `connector` block takes the attributes of its body \
         above, and the attributes and blocks of the kind that its `kind` \
         names.\n\n## `"
    ));
    let mut after = 0;
    for (name, body) in kinds.schemas() {
        let heading = format!("\n## `{name}`\n\n{}\n\n", body.summary);
        after += section[after..].find(&heading).expect(&heading) + heading.len();
        for attribute in body.attributes {
            let line = format!(
                "- `{}`: {}. {} {}\n",
                attribute.name,
                attribute.value,
                presence(attribute.presence),
                attribute.summary
            );
            assert!(section[after..].starts_with(&line), "{line}");
            after += line.len();
        }
        for inner in body.blocks {
            let heading = format!("\n### `{}`\n", inner.keyword);
            assert!(section[after..].contains(&heading), "{heading}");
        }
        for example in body.examples {
            let block = format!(
                "```hcl\nconnector \"{name}\" {{\n  kind = \"{name}\"\n  node = \
                 \"edge\"\n"
            );
            let at = section[after..].find(&block).expect(&block) + after;
            let fence = &section[at..];
            for line in example.lines().filter(|line| !line.is_empty()) {
                assert!(fence.contains(&format!("\n  {line}\n")), "{line}");
            }
        }
    }
    let stated = "\nExample:\n\n```hcl\nconnector \"stated\" {\n  kind = \
                  \"stated\"\n  node = \"edge\"\n\n  one = \"x\"\n\n  inner {\n  }\n}\n```\n";
    assert!(section.contains(stated), "{section}");
}

#[test]
fn an_empty_example_of_a_kind_is_a_connector_block_with_no_gap() {
    let docs = operation::docs(&Table::new().with("empty", Empty));
    let block = "```hcl\nconnector \"empty\" {\n  kind = \"empty\"\n  node = \
                 \"edge\"\n}\n```\n";
    assert!(docs.contains(block), "{docs}");
}

/// A kind whose one example holds a heredoc.
struct Heredoc;

impl connector::kind::Kind for Heredoc {
    type Config = ();
    const SCHEMA: Body = Body {
        summary: "A kind with a heredoc example.",
        attributes: &[],
        blocks: &[],
        examples: &["note = <<EOF\nline one\nline two\nEOF\n"],
    };

    fn parse(&self, _: &document::Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
        std::future::ready(Ok(()))
    }
}

#[test]
fn a_heredoc_example_keeps_its_value_in_the_connector_block() {
    let docs = operation::docs(&Table::new().with("heredoc", Heredoc));
    let start = docs
        .find("```hcl\nconnector \"heredoc\"")
        .expect("the block")
        + 7;
    let fence = &docs[start..];
    let fence = &fence[..fence.find("```\n").expect("a closed fence")];
    let file = config_hcl::read(document::Source(0), fence).expect(fence);
    let stored = config_hcl::read(
        document::Source(0),
        <Heredoc as connector::kind::Kind>::SCHEMA.examples[0],
    )
    .expect("the example");
    let value =
        |d: &document::Document| d.attributes.get("note").map(|a| a.value.kind.clone());
    assert_eq!(value(&file.blocks[0].body), value(&stored), "{fence}");
}

#[test]
fn config_takes_each_kind_example_block_alone_as_a_file() {
    let kinds = Table::new()
        .with("stated", Stated)
        .with("empty", Empty)
        .with("heredoc", Heredoc);
    let docs = operation::docs(&kinds);
    let start = docs.find("\n# Connector kinds\n").expect("a kinds section");
    let end = docs.find("\n# Secret sources\n").expect("a secret section");
    let fences: Vec<_> = docs[start..end]
        .split("```hcl\n")
        .skip(1)
        .map(|fence| &fence[..fence.find("```\n").expect("a closed fence")])
        .collect();
    assert_eq!(fences.len(), 3);
    for fence in fences {
        let file = config_hcl::read(document::Source(0), fence).expect(fence);
        assert_eq!(config::check(&[file], &kinds).err(), None, "{fence}");
    }
}
