//! The `foundation` binary. It runs the command line; starting a node comes later.

use std::io;
use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(ops::cli(
        std::env::args_os(),
        io::stdin().lock(),
        io::stdout().lock(),
        io::stderr().lock(),
    ))
}
