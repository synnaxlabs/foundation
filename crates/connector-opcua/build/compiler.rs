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

/// The build of the open62541 copy at `copy` and the build of the shim, each with
/// the flags of `flags`, the text of the copy's `flags.txt`. Each include path is
/// relative to the copy. The shim is our code, so its warnings are errors.
pub(crate) fn builds(copy: &Path, flags: &str) -> (Build, Build) {
    let mut library = Build::new();
    library.warnings(false);
    let mut shim = Build::new();
    shim.warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true);
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
    (library, shim)
}
