use std::path::Path;

/// Refuses the compiler at `path` unless it reads GCC driver flags, the form of each
/// flag in `flags.txt`. GCC and clang do. MSVC warns on each and goes on.
pub(crate) fn check(path: &Path, reads_gcc_flags: bool) -> Result<(), String> {
    if reads_gcc_flags {
        return Ok(());
    }
    Err(format!(
        "connector-opcua: the compiler {} is not GCC or clang; flags.txt holds GCC \
         driver flags, so only GCC and clang can build open62541",
        path.display()
    ))
}
