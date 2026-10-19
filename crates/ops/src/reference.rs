//! The reference of what a config file holds: each block, the body of each connector
//! kind, and each source of a secret.

use connector::kind::Table;
use document::schema::{Attribute, Block, Body, Presence};
use secret::{CALLS, Call};

/// Each of `blocks`, the body of each kind of `kinds`, and each call of [`CALLS`], as
/// Markdown.
pub(crate) fn markdown(blocks: &[Block], kinds: &Table) -> String {
    let blocks: String = blocks.iter().map(|b| block(b, 2)).collect();
    let kinds: String = kinds.schemas().map(|(name, b)| kind(name, b)).collect();
    let calls: String = CALLS.iter().map(call).collect();
    format!(
        "\n# Config blocks\n{blocks}\n# Connector kinds\n\nA `connector` block takes the \
         attributes of its body above, and the attributes and blocks of the kind that \
         its `kind` names.\n{kinds}\n# Secret sources\n\nA secret attribute takes one \
         of these calls, which names where its value comes from, never the \
         value.\n\n{calls}"
    )
}

/// The node of each example of a kind.
const NODE: &str = "edge";

/// The body of the kind `name`, under a heading of level 2, with each example inside a
/// whole `connector` block, so that a copy of it is a block that a config takes.
fn kind(name: &str, body: &Body) -> String {
    let examples = body.examples.iter().map(|example| {
        // The lines of a heredoc are its value, so an example that may hold one keeps
        // its lines as they are.
        let indent = if example.contains("<<") { "" } else { "  " };
        let lines: String = example
            .lines()
            .map(|line| match line {
                "" => "\n".to_owned(),
                line => format!("{indent}{line}\n"),
            })
            .collect();
        let gap = if lines.is_empty() { "" } else { "\n" };
        format!(
            "connector \"{name}\" {{\n  kind = \"{name}\"\n  node = \"{NODE}\"\n{gap}\
             {lines}}}\n"
        )
    });
    format!("\n## `{name}`\n{}", self::body(body, 3, examples))
}

/// `block` under a heading of `level`, with each of its forms.
fn block(block: &Block, level: usize) -> String {
    let label = block.label.map_or_else(
        || "No label.".to_owned(),
        |label| format!("Label: {label}."),
    );
    let examples = |form: &Body| form.examples.iter().map(|e| (*e).to_owned());
    let forms = match block.forms {
        [form] => body(form, level + 1, examples(form)),
        forms => forms
            .iter()
            .enumerate()
            .map(|(n, form)| {
                let heading = "#".repeat(level + 1);
                let body = body(form, level + 2, examples(form));
                format!("\n{heading} Form {}\n{body}", n + 1)
            })
            .collect::<Vec<_>>()
            .concat(),
    };
    format!(
        "\n{} `{}`\n\n{label}\n{forms}",
        "#".repeat(level),
        block.keyword
    )
}

/// `body` with `examples`, its examples as the docs give them, and then each block
/// inside it under a heading of `level`, so no example is under the heading of a block.
fn body(body: &Body, level: usize, examples: impl Iterator<Item = String>) -> String {
    let attributes: String = body.attributes.iter().map(attribute).collect();
    let blocks: String = body.blocks.iter().map(|b| block(b, level)).collect();
    let examples = examples
        .map(|example| format!("\nExample:\n\n```hcl\n{example}```\n"))
        .collect::<Vec<_>>()
        .concat();
    format!("\n{}\n\n{attributes}{examples}{blocks}", body.summary)
}

/// One line: the name, the value, when the check needs it, and what it sets.
fn attribute(attribute: &Attribute) -> String {
    format!(
        "- `{}`: {}. {} {}\n",
        attribute.name,
        attribute.value,
        presence(attribute.presence),
        attribute.summary
    )
}

/// When the check needs an attribute, as one sentence.
fn presence(presence: Presence) -> String {
    match presence {
        Presence::Required => "Required.".to_owned(),
        Presence::Unless(others) => {
            let names: Vec<String> = others.iter().map(|o| format!("`{o}`")).collect();
            let names = match names.as_slice() {
                [.., last] if names.len() > 1 => {
                    let rest = names[..names.len() - 1].join(", ");
                    let and = if names.len() > 2 { ", and" } else { " and" };
                    format!("{rest}{and} {last}")
                }
                _ => names.concat(),
            };
            format!("Required unless the body holds {names}.")
        }
        Presence::Default(value) => format!("Default: {value}."),
        Presence::Optional => "Optional.".to_owned(),
    }
}

/// One line: the call with its argument, and where the value comes from.
fn call(call: &Call) -> String {
    format!("- `{call}`: {}\n", call.summary())
}

#[cfg(test)]
mod tests;
