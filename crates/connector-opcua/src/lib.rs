//! Reads, subscribes to, and writes OPC UA servers through open62541, compiled in with
//! the feature `open62541`.

#[cfg(feature = "open62541")]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "part 2 of #435 gives the kind that uses it")
)]
mod event;
#[cfg(feature = "open62541")]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "part 2 of #435 gives the kind that uses it")
)]
mod ffi;
#[cfg(test)]
#[cfg(feature = "open62541")]
mod link;

// `build.rs` calls it, and a build script has no test harness.
#[cfg(test)]
#[path = "../build/compiler.rs"]
mod compiler;

#[cfg(test)]
mod tests {
    use super::compiler;

    /// Gives the tool that `cc` picks for `path`. No such file exists, so `cc` takes
    /// the family from the name, as it does for a compiler it cannot run.
    fn tool(path: &str) -> cc::Tool {
        cc::Build::new()
            .compiler(path)
            .target("x86_64-unknown-linux-gnu")
            .host("x86_64-unknown-linux-gnu")
            .opt_level(0)
            .cargo_metadata(false)
            .cargo_warnings(false)
            .get_compiler()
    }

    #[test]
    fn check_refuses_a_compiler_like_msvc() {
        for path in ["/missing/cl.exe", "/missing/clang-cl"] {
            assert_eq!(
                compiler::check(&tool(path)),
                Err(format!(
                    "connector-opcua: the compiler {path} is like MSVC; flags.txt holds \
                     GCC driver flags, which it does not read, so it cannot build \
                     open62541"
                ))
            );
        }
    }

    #[test]
    fn check_accepts_gcc_and_clang() {
        for path in ["/missing/gcc", "/missing/clang"] {
            assert_eq!(compiler::check(&tool(path)), Ok(()));
        }
    }
}
