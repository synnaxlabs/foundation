//! Repository tasks, run as `cargo xtask <task>`.

#![allow(clippy::print_stderr, reason = "xtask reports to the terminal")]

mod map;

use std::collections::BTreeSet;
use std::process::{Command, ExitCode};

use serde_json::Value;

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() != Some("layers") {
        eprintln!("usage: cargo xtask layers");
        return ExitCode::FAILURE;
    }
    match layers() {
        Ok(()) => ExitCode::SUCCESS,
        Err(problems) => {
            for problem in problems {
                eprintln!("error: {problem}\n");
            }
            ExitCode::FAILURE
        }
    }
}

/// Checks every workspace dependency against the crate map in `map.rs`.
fn layers() -> Result<(), Vec<String>> {
    let metadata = metadata().map_err(|e| vec![e])?;
    let packages = metadata["packages"].as_array().cloned().unwrap_or_default();
    let members: BTreeSet<&str> =
        packages.iter().filter_map(|p| p["name"].as_str()).collect();
    let mut problems = Vec::new();
    for package in &packages {
        let Some(name) = package["name"].as_str() else {
            continue;
        };
        if name == "xtask" {
            continue;
        }
        let Some(entry) = map::find(name) else {
            problems.push(format!(
                "crate `{name}` is not in the crate map. Add it to xtask/src/map.rs with \
                 its layer, its job, and its allowed dependencies, matching the crate \
                 map in docs/decisions.md."
            ));
            continue;
        };
        let deps = package["dependencies"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        for dep in &deps {
            let Some(dep_name) = dep["name"].as_str() else {
                continue;
            };
            if !members.contains(dep_name) {
                continue;
            }
            let dev = dep["kind"].as_str() == Some("dev");
            if entry.allows(dep_name) || (dev && map::TEST_ONLY.contains(&dep_name)) {
                continue;
            }
            problems.push(violation(entry, dep_name));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems)
    }
}

fn violation(entry: &map::Crate, dep: &str) -> String {
    let dep_layer =
        map::find(dep).map_or_else(|| "?".to_string(), |d| d.layer.to_string());
    let allowed = entry.describe();
    format!(
        "`{}` (layer {}) depends on `{dep}` (layer {dep_layer}).\n  `{}` may depend on: \
         {allowed}.\n  Fix: move the shared code into a lower crate. Layer 3 reaches \
         the core only through `hub`. A change to the map is an interface change: \
         see docs/coordination.md.",
        entry.name, entry.layer, entry.name
    )
}

fn metadata() -> Result<Value, String> {
    #[allow(clippy::disallowed_methods, reason = "cargo sets CARGO for its tools")]
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .map_err(|e| format!("cargo metadata: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("cargo metadata: {e}"))
}
