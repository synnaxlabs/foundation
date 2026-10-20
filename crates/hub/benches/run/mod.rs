//! The lines that a `hub` bench runs and their rounds, from `-- --line <prefix>
//! --rounds <n>`, so that a profile can run one line for many rounds.

/// The lines to run and their rounds.
pub(crate) struct Run {
    /// The prefix of the name of each line to run, or all lines.
    line: Option<String>,
    pub(crate) rounds: usize,
}

impl Run {
    /// The run that the arguments give, with `rounds` rounds unless `--rounds` gives
    /// another count. In the prefix of `--line`, `_` stands for a space. Ignores
    /// `--bench`, which cargo adds.
    ///
    /// # Panics
    ///
    /// On an unknown argument, or an argument with no value.
    pub(crate) fn new(rounds: usize) -> Self {
        let mut run = Self { line: None, rounds };
        #[expect(clippy::disallowed_methods, reason = "the bench reads its lines")]
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--line" => {
                    let line = args.next().expect("a line prefix");
                    run.line = Some(line.replace('_', " "));
                }
                "--rounds" => {
                    let rounds = args.next().expect("a count of rounds");
                    run.rounds = rounds.parse().expect("a count of rounds");
                }
                "--bench" => {}
                other => panic!("unknown argument {other}"),
            }
        }
        run
    }

    /// Whether the run holds the line `name`.
    pub(crate) fn runs(&self, name: &str) -> bool {
        self.line
            .as_ref()
            .is_none_or(|line| name.starts_with(line.as_str()))
    }
}
