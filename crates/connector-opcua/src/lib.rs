//! Reads, subscribes to, and writes OPC UA servers through open62541, compiled in with
//! the feature `open62541`.

// Only `link` calls it until the event loop of #435 does.
#[cfg(test)]
#[cfg(feature = "open62541")]
mod ffi;
#[cfg(test)]
#[cfg(feature = "open62541")]
mod link;

#[cfg(test)]
mod child;

// `build.rs` calls it, and a build script has no test harness.
#[cfg(test)]
#[path = "../build/compiler.rs"]
mod compiler;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{child, compiler};

    /// The target of each build that these tests make.
    const TARGET: &str = "x86_64-unknown-linux-gnu";

    /// Gives the tool that `build` picks for `path`. No such file exists, so `cc`
    /// takes the family from the name, as it does for a compiler it cannot run.
    fn tool(mut build: cc::Build, path: &str) -> cc::Tool {
        build
            .compiler(path)
            .target(TARGET)
            .host(TARGET)
            .opt_level(0)
            .cargo_metadata(false)
            .cargo_warnings(false)
            .get_compiler()
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
                compiler::check(&tool(cc::Build::new(), path)),
                Err(format!(
                    "connector-opcua: the compiler {path} is like MSVC; flags.txt \
                     holds GCC driver flags, which it does not read, so it cannot \
                     build open62541"
                ))
            );
        }
    }

    #[test]
    fn check_accepts_gcc_and_clang() {
        for path in ["/missing/gcc", "/missing/clang"] {
            assert_eq!(compiler::check(&tool(cc::Build::new(), path)), Ok(()));
        }
    }

    /// The directories that `-I` gives in `args`.
    fn includes(args: &[String]) -> Vec<&str> {
        let pairs = args.windows(2).filter(|pair| pair[0] == "-I");
        pairs.map(|pair| pair[1].as_str()).collect()
    }

    /// Checks the builds in the environment of the process, and does nothing outside a
    /// child process. `cc` adds its default warnings only when no `CFLAGS` is
    /// set, so `the_shim_fails_on_a_warning_with_or_without_cflags` runs it in child
    /// processes with each environment.
    #[test]
    fn builds_in_this_environment() {
        if !child::running() {
            return;
        }
        let copy = Path::new("/copy");
        let compiler::Builds { library, shim } =
            compiler::builds(copy, "-Ideps\n-Iinclude\n-std=c99", "a.c\nsrc/b.c");
        assert_eq!(
            library.get_files().collect::<Vec<_>>(),
            [copy.join("a.c"), copy.join("src/b.c")]
        );
        assert_eq!(
            shim.get_files().collect::<Vec<_>>(),
            [Path::new(env!("CARGO_MANIFEST_DIR")).join("src/shim.c")]
        );
        let dirs = ["/copy/deps", "/copy/include"];
        for path in ["/missing/gcc", "/missing/clang"] {
            let shim = args(&tool(shim.clone(), path));
            assert_eq!(includes(&shim), dirs, "{path}: {shim:?}");
            for arg in ["-Wall", "-Wextra", "-Werror", "-std=c99"] {
                assert!(shim.iter().any(|a| a == arg), "{path}: {arg} in {shim:?}");
            }
            let library = args(&tool(library.clone(), path));
            assert_eq!(includes(&library), dirs, "{path}: {library:?}");
            assert!(
                library.iter().any(|a| a == "-std=c99"),
                "{path}: {library:?}"
            );
            for arg in ["-Wall", "-Wextra", "-Werror"] {
                assert!(
                    !library.contains(&arg.into()),
                    "{path}: {arg} in {library:?}"
                );
            }
        }
    }

    #[test]
    fn the_shim_fails_on_a_warning_with_or_without_cflags() {
        for cflags in [None, Some("-O1")] {
            child::run("tests::builds_in_this_environment", TARGET, cflags);
        }
    }
}
