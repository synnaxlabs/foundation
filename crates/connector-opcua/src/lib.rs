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
    expect(dead_code, reason = "only `bench` uses it until the session of #435")
)]
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

    /// The tool of this process for its own target, with the compiler and the `CFLAGS`
    /// of its environment.
    fn probe() -> cc::Tool {
        child::tool(&mut cc::Build::new(), env!("CONNECTOR_OPCUA_TARGET"))
    }

    #[test]
    fn asan_is_true_in_a_child_process() {
        if child::running() {
            assert_eq!(compiler::asan(&probe()), Ok(true));
        }
    }

    #[test]
    fn asan_is_false_in_a_child_process() {
        if child::running() {
            assert_eq!(compiler::asan(&probe()), Ok(false));
        }
    }

    /// Gives the error of `asan` for `tool` with `reason`.
    fn cannot_preprocess(tool: &cc::Tool, reason: &str) -> String {
        let probe = Path::new(env!("CARGO_MANIFEST_DIR")).join(compiler::PROBE);
        format!(
            "connector-opcua: {} cannot preprocess {}: {reason}",
            tool.path().display(),
            probe.display()
        )
    }

    #[test]
    fn asan_fails_when_the_tool_cannot_run() {
        let tool = tool(cc::Build::new(), "/missing/cc");
        // ENOENT on Linux and macOS.
        let reason = std::io::Error::from_raw_os_error(2);
        assert_eq!(
            compiler::asan(&tool),
            Err(cannot_preprocess(&tool, &reason.to_string()))
        );
    }

    #[test]
    fn asan_fails_when_the_tool_cannot_preprocess() {
        let tool = tool(cc::Build::new(), "false");
        assert_eq!(compiler::asan(&tool), Err(cannot_preprocess(&tool, "")));
    }

    #[test]
    fn asan_fails_with_the_stderr_of_the_tool() {
        let mut build = cc::Build::new();
        build.flag("--connector-opcua-no-such-flag");
        let tool = child::tool(&mut build, env!("CONNECTOR_OPCUA_TARGET"));
        let message = compiler::asan(&tool).unwrap_err();
        let stderr = message
            .strip_prefix(&cannot_preprocess(&tool, ""))
            .unwrap_or_else(|| panic!("{message}"));
        assert!(
            stderr.contains("--connector-opcua-no-such-flag"),
            "{message}"
        );
    }

    #[test]
    fn asan_is_true_only_with_address_sanitizer_in_cflags() {
        let (asan, other) = (
            "tests::asan_is_true_in_a_child_process",
            "tests::asan_is_false_in_a_child_process",
        );
        let cases = [
            (asan, Some("-fsanitize=address")),
            (asan, Some("-O1 -fsanitize=undefined,address")),
            (other, None),
            (other, Some("-fsanitize=undefined")),
            (other, Some("-fsanitize=address -fno-sanitize=address")),
        ];
        // Clang 18 defines no `__SANITIZE_ADDRESS__`; only `__has_feature` finds it.
        for compiler in [None, Some("clang")] {
            for (name, cflags) in cases {
                let compiler = compiler.map(|compiler| ("CC", compiler));
                let cflags = cflags.map(|cflags| ("CFLAGS", cflags));
                let envs: Vec<_> = compiler.into_iter().chain(cflags).collect();
                child::run(name, &envs);
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
