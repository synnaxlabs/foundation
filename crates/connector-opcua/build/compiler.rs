use cc::Tool;

/// Refuses `tool` unless it reads GCC driver flags, the form of each flag in
/// `flags.txt`. GCC and clang do. MSVC and clang-cl warn on each and go on.
pub(crate) fn check(tool: &Tool) -> Result<(), String> {
    if tool.is_like_gnu() || tool.is_like_clang() {
        return Ok(());
    }
    Err(format!(
        "connector-opcua: the compiler {} is not GCC or clang; flags.txt holds GCC \
         driver flags, so only GCC and clang can build open62541",
        tool.path().display()
    ))
}
