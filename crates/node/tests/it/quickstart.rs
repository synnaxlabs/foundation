//! Checks the quickstart page against the binary.

use crate::{foundation, text};

const PAGE: &str = include_str!("../../../../docs/quickstart.md");

/// The text of each fenced block on the page whose language is `language`.
fn blocks(language: &str) -> impl Iterator<Item = &'static str> + '_ {
    PAGE.split("```")
        .skip(1)
        .step_by(2)
        .filter_map(move |block| block.strip_prefix(language)?.strip_prefix('\n'))
}

#[test]
fn the_help_names_each_command_and_flag_on_the_quickstart_page() {
    let commands: Vec<Vec<&str>> = blocks("sh")
        .flat_map(str::lines)
        .filter_map(|line| line.strip_prefix("foundation "))
        .map(|words| words.split_whitespace().collect())
        .collect();
    assert!(!commands.is_empty(), "the page has no `foundation` command");
    for words in &commands {
        let output = foundation(&[words[0], "--help"], "");
        let help = text(&output.stdout);
        assert_eq!(
            output.status.code(),
            Some(0),
            "`foundation {} --help` failed: {}",
            words[0],
            text(&output.stderr)
        );
        for flag in words.iter().filter(|word| word.starts_with("--")) {
            assert!(
                help.split_whitespace()
                    .any(|word| word.trim_end_matches(',') == *flag),
                "`foundation {} --help` does not name `{flag}`:\n{help}",
                words[0]
            );
        }
    }
}
