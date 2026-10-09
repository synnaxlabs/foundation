//! The pinned nightly toolchain, which Miri and cargo-fuzz use.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// The toolchain in `rust-toolchain-nightly` of a workspace, which Miri and cargo-fuzz
/// use.
pub(crate) struct Toolchain {
    root: PathBuf,
    pin: String,
}

impl Toolchain {
    /// The toolchain in `rust-toolchain-nightly` of the workspace at `root`.
    pub(crate) fn read(root: &Path) -> Result<Self, String> {
        let pin = root.join("rust-toolchain-nightly");
        let nightly = std::fs::read_to_string(&pin)
            .map_err(|e| format!("{}: {e}", pin.display()))?;
        Ok(Self {
            root: root.to_path_buf(),
            pin: nightly.trim().to_string(),
        })
    }

    /// A command that runs cargo of this toolchain through rustup, at the workspace
    /// root.
    pub(crate) fn cargo(&self) -> Command {
        self.tool("cargo")
    }

    /// The host triple of this toolchain, or the error of `rustc -vV`.
    pub(crate) fn host(&self) -> Result<String, String> {
        let command = format!("`rustup run {} rustc -vV`", self.pin);
        let output = self
            .tool("rustc")
            .arg("-vV")
            .output()
            .map_err(|e| format!("{command}: {e}"))?;
        parse_host(&output).map_err(|e| format!("{command}: {e}"))
    }

    /// The toolchain `pin` of the workspace at `root`.
    #[cfg(test)]
    pub(crate) fn new(root: &str, pin: &str) -> Self {
        Self {
            root: root.into(),
            pin: pin.to_string(),
        }
    }

    /// A command that runs `tool` of this toolchain through rustup, at the workspace
    /// root.
    fn tool(&self, tool: &str) -> Command {
        let mut command = Command::new("rustup");
        command
            .current_dir(&self.root)
            .args(["run", &self.pin, tool]);
        command
    }
}

/// The host triple in `output`, from `rustc -vV`, or its exit status and stderr when it
/// failed. The error does not name the command, which the caller adds.
fn parse_host(output: &Output) -> Result<String, String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("{}: {}", output.status, stderr.trim()));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines()
        .find_map(|line| line.strip_prefix("host: "))
        .map(str::to_string)
        .ok_or_else(|| format!("gives no host: {text}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::output;
    use crate::fixture;

    #[test]
    fn runs_cargo_of_the_pin_at_the_root() {
        let cargo = Toolchain::read(&fixture()).unwrap().cargo();
        let args: Vec<_> = cargo.get_args().collect();
        assert_eq!(cargo.get_program(), "rustup");
        assert_eq!(args, ["run", "nightly-2000-01-01", "cargo"]);
        assert_eq!(cargo.get_current_dir(), Some(fixture().as_path()));
    }

    #[test]
    fn names_a_missing_pin() {
        let root = fixture().join("stale");
        assert_eq!(
            Toolchain::read(&root).err(),
            Some(format!(
                "{}: No such file or directory (os error 2)",
                root.join("rust-toolchain-nightly").display()
            ))
        );
    }

    #[test]
    fn parse_host_reads_the_host_of_rustc() {
        let text = "rustc 1.93.0-nightly (abc 2026-10-01)\nbinary: rustc\n\
                    host: x86_64-unknown-linux-gnu\nrelease: 1.93.0-nightly\n";
        assert_eq!(
            parse_host(&output(0, text, "")),
            Ok("x86_64-unknown-linux-gnu".into())
        );
        let text = "rustc 1.93.0-nightly\nrelease: 1.93.0\n";
        assert_eq!(
            parse_host(&output(0, text, "")),
            Err(format!("gives no host: {text}"))
        );
    }

    #[test]
    fn reads_the_host_of_the_pin() {
        let root = std::env::temp_dir()
            .join(format!("xtask-nightly-host-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let toolchain = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../rust-toolchain.toml"),
        )
        .unwrap();
        let channel = toolchain
            .lines()
            .find_map(|line| line.strip_prefix("channel = "))
            .unwrap()
            .trim_matches('"');
        std::fs::write(root.join("rust-toolchain-nightly"), channel).unwrap();
        let host = Toolchain::read(&root).unwrap().host();
        std::fs::remove_dir_all(&root).unwrap();
        let host = host.unwrap();
        assert!(host.starts_with(std::env::consts::ARCH), "{host}");
    }

    #[test]
    fn parse_host_names_the_error_of_rustc() {
        let error = "error: toolchain 'nightly-x' is not installed\n";
        assert_eq!(
            parse_host(&output(1, "", error)),
            Err("exit status: 1: error: toolchain 'nightly-x' is not installed".into())
        );
        assert_eq!(
            parse_host(&output(2, "", "")),
            Err("exit status: 2: ".into())
        );
    }

    #[test]
    fn names_rustup_and_the_pin_when_the_pin_is_not_installed() {
        let error = Toolchain::read(&fixture()).unwrap().host().unwrap_err();
        let command = "`rustup run nightly-2000-01-01 rustc -vV`: exit status: 1: ";
        assert!(error.starts_with(command), "{error}");
        assert!(error.contains("'nightly-2000-01-01"), "{error}");
    }
}
