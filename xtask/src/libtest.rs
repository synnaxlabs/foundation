//! Runs a `cargo test` and reads what libtest reports.

use std::process::{Command, Stdio};

/// How a run of libtest ended.
#[derive(Debug, PartialEq)]
pub(crate) enum Run {
    /// The command exited with an error.
    Failed,
    /// The command passed and ran no test.
    Empty,
    /// The command passed and ran at least one test.
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
    } else if tests_ran(&stdout) == 0 {
        Run::Empty
    } else {
        Run::Passed
    })
}

/// The sum of N over the `running N tests` lines of libtest output.
fn tests_ran(output: &str) -> usize {
    output
        .lines()
        .filter_map(|line| line.strip_prefix("running ")?.split(' ').next())
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
        assert_eq!(sh("echo 'running 2 tests'"), Ok(Run::Passed));
        assert_eq!(sh("echo 'running 0 tests'"), Ok(Run::Empty));
        assert_eq!(sh("echo 'running 2 tests'; exit 101"), Ok(Run::Failed));
    }

    #[test]
    fn names_a_program_that_cannot_start() {
        assert_eq!(
            run(&mut Command::new("/missing/cargo")),
            Err("/missing/cargo: No such file or directory (os error 2)".to_string())
        );
    }

    #[test]
    fn tests_ran_sums_each_test_binary() {
        let output = "\nrunning 2 tests\ntest a ... ok\n\nrunning 0 tests\n\n\
                      running 1 test\ntest b ... ok\n";
        assert_eq!(tests_ran(output), 3);
    }

    #[test]
    fn tests_ran_is_zero_with_no_tests() {
        assert_eq!(tests_ran("\nrunning 0 tests\n\ntest result: ok.\n"), 0);
    }
}
