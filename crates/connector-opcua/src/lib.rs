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

    /// The arguments that `after` adds in one place to `before`.
    fn added<'a>(before: &[String], after: &'a [String]) -> Vec<&'a str> {
        let common = |a: &[String], b: &[String]| {
            a.iter().zip(b).take_while(|(a, b)| a == b).count()
        };
        let start = common(before, after);
        let rest = &before[start..];
        let end = common(
            &rest.iter().rev().cloned().collect::<Vec<_>>(),
            &after.iter().rev().cloned().collect::<Vec<_>>(),
        )
        .min(rest.len());
        assert_eq!(
            [&after[..start], &after[after.len() - end..]].concat(),
            before
        );
        after[start..after.len() - end]
            .iter()
            .map(String::as_str)
            .collect()
    }

    /// The variables of a build script whose target has the `cfg(sanitize)` list
    /// `sanitize`, and `cfg(fuzzing)` when `fuzzing`.
    fn cfgs(sanitize: &str, fuzzing: bool) -> impl Fn(&str) -> Option<String> {
        move |name| match name {
            "CARGO_CFG_SANITIZE" if !sanitize.is_empty() => Some(sanitize.to_string()),
            "CARGO_CFG_FUZZING" if fuzzing => Some(String::new()),
            _ => None,
        }
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
            ("leak", false, vec![]),
            ("", true, vec![fuzzer]),
            ("address", true, [&address[..], &[fuzzer]].concat()),
        ];
        for (sanitize, fuzzing, expected) in cases {
            let (mut plain, mut sanitized) =
                (builds("/missing/clang"), builds("/missing/clang"));
            let asan = sanitized.sanitize(cfgs(sanitize, fuzzing));
            let expected_asan = expected.contains(&address[0]);
            assert_eq!(asan, Ok(expected_asan), "{sanitize} {fuzzing}");
            let pairs = [
                (&mut sanitized.library, &mut plain.library),
                (&mut sanitized.shim, &mut plain.shim),
            ];
            for (build, plain) in pairs {
                let tool = child::tool(build, TARGET);
                assert_eq!(tool.path(), Path::new("/missing/clang"));
                let before = args(&child::tool(plain, TARGET));
                let after = args(&tool);
                assert_eq!(added(&before, &after), expected, "{sanitize} {fuzzing}");
            }
        }
    }

    /// `cc` adds the `CFLAGS` of the caller, which `sanitize_follows_the_rust_build`
    /// does not check.
    #[test]
    fn sanitize_follows_the_rust_build_with_cflags() {
        child::run(
            "tests::sanitize_follows_the_rust_build",
            &[("CFLAGS", "-fno-sanitize-recover=all")],
        );
    }

    #[test]
    fn sanitize_refuses_a_sanitizer_that_the_c_does_not_follow() {
        for (sanitize, name) in [
            ("memory", "memory"),
            ("hwaddress", "hwaddress"),
            ("thread", "thread"),
            ("addressx", "addressx"),
            ("leak,memory", "memory"),
            ("memory,address", "memory"),
        ] {
            let (mut plain, mut sanitized) =
                (builds("/missing/gcc"), builds("/missing/gcc"));
            assert_eq!(
                sanitized.sanitize(cfgs(sanitize, true)),
                Err(format!(
                    "connector-opcua: the C does not build with the sanitizer `{name}` \
                     of the Rust build; it follows only `address` and `leak`"
                ))
            );
            let tool = child::tool(&mut sanitized.library, TARGET);
            assert_eq!(tool.path(), Path::new("/missing/gcc"));
            assert_eq!(args(&tool), args(&child::tool(&mut plain.library, TARGET)));
        }
    }

    #[test]
    fn sanitize_moves_gcc_to_clang() {
        for (sanitize, fuzzing) in [("address", false), ("", true)] {
            let mut builds = builds("/missing/gcc");
            assert_eq!(builds.sanitize(cfgs(sanitize, fuzzing)), Ok(!fuzzing));
            for build in [&mut builds.library, &mut builds.shim] {
                assert_eq!(child::tool(build, TARGET).path(), Path::new("clang"));
            }
        }
    }

    #[test]
    fn sanitize_keeps_gcc_when_the_c_has_no_sanitizer() {
        for sanitize in ["", "leak"] {
            let mut builds = builds("/missing/gcc");
            assert_eq!(builds.sanitize(cfgs(sanitize, false)), Ok(false));
            for build in [&mut builds.library, &mut builds.shim] {
                let path = child::tool(build, TARGET).path().to_owned();
                assert_eq!(path, Path::new("/missing/gcc"), "{sanitize}");
            }
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
