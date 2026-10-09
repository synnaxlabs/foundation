//! Reads, subscribes to, and writes OPC UA servers through open62541, compiled in with
//! the feature `open62541`.

#[cfg(feature = "open62541")]
mod alloc;
#[cfg(feature = "sim")]
#[doc(hidden)]
pub mod bench;
#[cfg(feature = "open62541")]
#[cfg_attr(
    not(feature = "sim"),
    expect(dead_code, reason = "only `bench` uses it until the session of #435")
)]
mod event;
#[cfg(feature = "open62541")]
#[cfg_attr(
    not(feature = "sim"),
    expect(
        dead_code,
        reason = "only `bench` and `fuzz` use it until the session of #435"
    )
)]
mod ffi;
#[cfg(feature = "sim")]
#[doc(hidden)]
pub mod fuzz;
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

    use super::child;
    use super::compiler;

    /// The target of each build that these tests make.
    const TARGET: &str = "x86_64-unknown-linux-gnu";

    /// Gives the tool that `build` picks for `path`. When `cc` cannot run `path` to
    /// find its family, it takes the family from the name.
    fn tool(mut build: cc::Build, path: &str) -> cc::Tool {
        child::tool(build.compiler(path), TARGET)
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

    /// The builds of `compiler::builds`, for `TARGET` and with the compiler `path`.
    fn builds(path: &str) -> compiler::Builds {
        let mut builds = compiler::builds(Path::new("/copy"), "-std=c99", "a.c");
        for build in [&mut builds.library, &mut builds.shim] {
            child::tool(build.compiler(path), TARGET);
        }
        builds
    }

    /// The flags of `args` that start with `-fsanitize` or `-fno-sanitize`.
    fn sanitizers(args: &[String]) -> Vec<&str> {
        let flags = args.iter().map(String::as_str);
        flags.filter(|arg| arg.contains("sanitize")).collect()
    }

    #[test]
    fn sanitize_follows_the_rust_build() {
        let address = [
            "-fsanitize=address,undefined",
            "-fno-sanitize=function",
            "-fno-sanitize-recover=all",
        ];
        let fuzzer = "-fsanitize=fuzzer-no-link";
        let cases = [
            ("", false, vec![]),
            ("address", false, address.to_vec()),
            ("leak,address", false, address.to_vec()),
            ("memory", false, vec![]),
            ("addressx", false, vec![]),
            ("", true, vec![fuzzer]),
            ("address", true, [&address[..], &[fuzzer]].concat()),
        ];
        for (sanitize, fuzzing, expected) in cases {
            let mut builds = builds("/missing/clang");
            let asan = builds.sanitize(sanitize, fuzzing);
            assert_eq!(asan, expected.contains(&address[0]), "{sanitize} {fuzzing}");
            for build in [&mut builds.library, &mut builds.shim] {
                let tool = child::tool(build, TARGET);
                assert_eq!(tool.path(), Path::new("/missing/clang"));
                assert_eq!(sanitizers(&args(&tool)), expected, "{sanitize} {fuzzing}");
            }
        }
    }

    #[test]
    fn sanitize_moves_gcc_to_clang() {
        for (sanitize, fuzzing) in [("address", false), ("", true)] {
            let mut builds = builds("/missing/gcc");
            builds.sanitize(sanitize, fuzzing);
            for build in [&mut builds.library, &mut builds.shim] {
                assert_eq!(child::tool(build, TARGET).path(), Path::new("clang"));
            }
        }
    }

    #[test]
    fn sanitize_keeps_gcc_with_no_sanitizer() {
        let mut builds = builds("/missing/gcc");
        builds.sanitize("memory", false);
        for build in [&mut builds.library, &mut builds.shim] {
            assert_eq!(child::tool(build, TARGET).path(), Path::new("/missing/gcc"));
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
        let header = format!(
            "-DUA_ARCH_HEADER=\"{}/src/alloc.h\"",
            env!("CARGO_MANIFEST_DIR")
        );
        for path in ["/missing/gcc", "/missing/clang"] {
            let shim = args(&tool(shim.clone(), path));
            assert_eq!(
                includes(&shim),
                [dirs[0], dirs[1], "/copy/arch/common"],
                "{path}: {shim:?}"
            );
            for arg in ["-Wall", "-Wextra", "-Werror", "-std=c99", &header] {
                assert!(shim.iter().any(|a| a == arg), "{path}: {arg} in {shim:?}");
            }
            let library = args(&tool(library.clone(), path));
            assert!(library.contains(&header), "{path}: {library:?}");
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
        for envs in [&[][..], &[("CFLAGS", "-O1")]] {
            child::run("tests::builds_in_this_environment", envs);
        }
    }
}
