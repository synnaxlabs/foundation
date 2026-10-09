use std::path::Path;

use cc::{Build, Tool};

/// Refuses `tool` when `cc` takes it for MSVC or clang-cl. `flags.txt` holds GCC
/// driver flags, and those compilers warn on each one and go on.
pub(crate) fn check(tool: &Tool) -> Result<(), String> {
    if !tool.is_like_msvc() {
        return Ok(());
    }
    Err(format!(
        "connector-opcua: the compiler {} is like MSVC; flags.txt holds GCC driver \
         flags, which it does not read, so it cannot build open62541",
        tool.path().display()
    ))
}

/// The two builds of `build.rs`.
pub(crate) struct Builds {
    /// The open62541 copy, with its warnings off.
    pub(crate) library: Build,
    /// `src/shim.c`, our code, so its warnings are errors.
    pub(crate) shim: Build,
}

/// Gives the builds of the open62541 copy at `copy` and of `src/shim.c`. `flags` is
/// the text of the copy's `flags.txt`, whose include paths are relative to the copy.
/// `sources` is the text of `sources.txt`, one file of the copy on each line.
pub(crate) fn builds(copy: &Path, flags: &str, sources: &str) -> Builds {
    let mut library = Build::new();
    library.warnings(false);
    let mut shim = Build::new();
    shim.warnings(true).warnings_into_errors(true);
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let header = format!("\"{}\"", root.join("src/alloc.h").display());
    library.define("UA_ARCH_HEADER", header.as_str());
    shim.define("UA_ARCH_HEADER", header.as_str());
    for flag in flags.lines() {
        if let Some(dir) = flag.strip_prefix("-I") {
            let dir = copy.join(dir);
            library.include(&dir);
            shim.include(dir);
        } else {
            library.flag(flag);
            shim.flag(flag);
        }
    }
    for source in sources.lines() {
        library.file(copy.join(source));
    }
    shim.file(root.join("src/shim.c"));
    Builds { library, shim }
}

/// Whether `tool` compiles C with `-fsanitize=address`, by its compiler and its flags,
/// `CFLAGS` included.
///
/// # Panics
///
/// When `tool` cannot preprocess a file.
pub(crate) fn asan(tool: &Tool) -> bool {
    let probe = Path::new(env!("CARGO_MANIFEST_DIR")).join("build/asan.c");
    let output = tool.to_command().arg("-E").arg(&probe).output();
    let output = output.unwrap_or_else(|e| panic!("{}: {e}", tool.path().display()));
    assert!(
        output.status.success(),
        "connector-opcua: {} cannot preprocess {}: {}",
        tool.path().display(),
        probe.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output
        .stdout
        .split(|&byte| byte == b'\n')
        .any(|line| line.trim_ascii() == b"connector_opcua_asan")
}
