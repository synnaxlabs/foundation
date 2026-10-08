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
    use std::path::Path;

    use super::compiler;

    /// Gives the tool that `build` picks for `path`. No such file exists, so `cc`
    /// takes the family from the name, as it does for a compiler it cannot run.
    fn tool_of(mut build: cc::Build, path: &str) -> cc::Tool {
        build
            .compiler(path)
            .target("x86_64-unknown-linux-gnu")
            .host("x86_64-unknown-linux-gnu")
            .opt_level(0)
            .cargo_metadata(false)
            .cargo_warnings(false)
            .get_compiler()
    }

    fn tool(path: &str) -> cc::Tool {
        tool_of(cc::Build::new(), path)
    }

    fn args(tool: &cc::Tool) -> Vec<String> {
        let args = tool
            .args()
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned());
        args.collect()
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

    /// The directories that `-I` gives in `args`.
    fn includes(args: &[String]) -> Vec<&str> {
        let pairs = args.windows(2).filter(|pair| pair[0] == "-I");
        pairs.map(|pair| pair[1].as_str()).collect()
    }

    #[test]
    fn the_shim_reads_the_copy_with_i_and_fails_on_a_warning() {
        let flags = "-Ideps\n-Iinclude\n-std=c99";
        let dirs = ["/copy/deps", "/copy/include"];
        for path in ["/missing/gcc", "/missing/clang"] {
            let (library, shim) = compiler::builds(Path::new("/copy"), flags);
            let shim = args(&tool_of(shim, path));
            assert_eq!(includes(&shim), dirs, "{path}: {shim:?}");
            for arg in ["-Wall", "-Wextra", "-Werror", "-std=c99"] {
                assert!(shim.iter().any(|a| a == arg), "{path}: {arg} in {shim:?}");
            }
            let library = args(&tool_of(library, path));
            assert_eq!(includes(&library), dirs, "{path}: {library:?}");
            assert!(
                library.iter().any(|a| a == "-std=c99"),
                "{path}: {library:?}"
            );
            assert!(
                !library.iter().any(|a| a == "-Werror"),
                "{path}: {library:?}"
            );
        }
    }
}
