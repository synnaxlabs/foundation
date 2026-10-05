//! The `foundation` binary.

use std::ffi::OsString;
use std::io::{self, BufRead, Write};
use std::process::ExitCode;

fn main() -> io::Result<ExitCode> {
    let args: Vec<OsString> = std::env::args_os().collect();
    if args.len() == 2 && args[1] == "mcp" {
        serve(io::stdin().lock(), io::stdout().lock())?;
        return Ok(ExitCode::SUCCESS);
    }
    let exit = ops::cli(args);
    io::stdout().write_all(exit.stdout.as_bytes())?;
    io::stderr().write_all(exit.stderr.as_bytes())?;
    Ok(ExitCode::from(exit.status))
}

/// Answers each MCP message on `input` with one line on `output`, until `input` ends.
fn serve(input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    for line in input.lines() {
        if let Some(reply) = ops::mcp(&line?) {
            writeln!(output, "{reply}")?;
            // The client waits for each reply before it sends the next request.
            output.flush()?;
        }
    }
    Ok(())
}
