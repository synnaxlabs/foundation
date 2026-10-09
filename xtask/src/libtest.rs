//! Runs a `cargo test` and reads what libtest reports.

use std::process::{Command, Stdio};

/// How a run of libtest ended.
#[derive(Debug, PartialEq)]
pub(crate) enum Run {
    /// The command exited with an error.
    Failed,
    /// The command passed, and no test passed: none exists, or each is ignored.
    Empty,
    /// The command passed, and at least one test passed.
    Passed,
}

/// Runs `command`, which runs libtest, with its stderr shown, and then prints its
/// stdout. It fails with the program and the error when the command cannot start.
pub(crate) fn run(command: &mut Command) -> Result<Run, String> {
    let output = command
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("{}: {e}", command.get_program().to_string_lossy()))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    eprint!("{stdout}");
    Ok(if !output.status.success() {
        Run::Failed
    } else if tests_passed(&stdout) == 0 {
        Run::Empty
    } else {
        Run::Passed
    })
}

/// The sum of N over the `test result: <status>. N passed` lines of libtest output.
fn tests_passed(output: &str) -> usize {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("test result: ")?.split(' ').nth(1))
        .filter_map(|count| count.parse::<usize>().ok())
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Result<Run, String> {
        run(Command::new("sh").args(["-c", script]))
    }

    #[test]
    fn reads_how_the_run_ended() {
        let passed = "echo 'test result: ok. 2 passed; 0 failed; 0 ignored'";
        assert_eq!(sh(passed), Ok(Run::Passed));
        assert_eq!(sh("echo 'test result: ok. 0 passed'"), Ok(Run::Empty));
        assert_eq!(sh(&format!("{passed}; exit 101")), Ok(Run::Failed));
        assert_eq!(sh("exit 101"), Ok(Run::Failed));
    }

    #[test]
    fn a_run_whose_tests_are_all_ignored_ran_no_test() {
        let script = "printf 'running 2 tests\\ntest a ... ignored\\ntest b ... \
                      ignored\\n\\ntest result: ok. 0 passed; 0 failed; 2 ignored; \
                      0 measured; 0 filtered out\\n'";
        assert_eq!(sh(script), Ok(Run::Empty));
    }

    #[test]
    fn names_a_program_that_cannot_start() {
        assert_eq!(
            run(&mut Command::new("/missing/cargo")),
            Err("/missing/cargo: No such file or directory (os error 2)".to_string())
        );
    }

    #[test]
    fn tests_passed_sums_each_test_binary() {
        let output = "\nrunning 2 tests\ntest a ... ok\ntest c ... ignored\n\n\
                      test result: ok. 1 passed; 0 failed; 1 ignored\n\n\
                      running 0 tests\n\ntest result: ok. 0 passed; 0 failed\n\n\
                      running 1 test\ntest b ... ok\n\n\
                      test result: ok. 1 passed; 0 failed\n";
        assert_eq!(tests_passed(output), 2);
    }
}
