//! Reads, subscribes to, and writes OPC UA servers through open62541, compiled in with
//! the feature `open62541`.

#[cfg(feature = "open62541")]
mod alloc;
#[cfg(feature = "sim")]
#[doc(hidden)]
pub mod bench;
#[cfg(feature = "open62541")]
mod browse;
#[cfg(feature = "open62541")]
mod client;
#[cfg(feature = "open62541")]
mod config;
#[cfg(feature = "open62541")]
mod connection;
#[cfg(feature = "open62541")]
mod event;
#[cfg(feature = "open62541")]
#[cfg_attr(
    not(feature = "sim"),
    expect(
        dead_code,
        reason = "only `bench` and `fuzz` use the codec of open62541"
    )
)]
mod ffi;
#[cfg(feature = "sim")]
#[doc(hidden)]
pub mod fuzz;
#[cfg(feature = "open62541")]
mod kind;
#[cfg(test)]
#[cfg(feature = "open62541")]
mod link;
#[cfg(feature = "open62541")]
mod node;
#[cfg(test)]
#[cfg(feature = "open62541")]
mod pem;
#[cfg(feature = "open62541")]
#[cfg(any(test, feature = "sim"))]
mod server;
#[cfg(feature = "sim")]
pub mod sim;
#[cfg(feature = "open62541")]
#[cfg(any(test, feature = "sim"))]
mod unwind;
#[cfg(feature = "open62541")]
mod value;

// The copy calls the libcrypto of `aws-lc-sys`, and no Rust here names the crate, which
// rustc links only when it is named.
#[cfg(feature = "open62541")]
use aws_lc_sys as _;
#[cfg(feature = "open62541")]
pub use config::Config;
#[cfg(feature = "open62541")]
pub use kind::Kind;

#[cfg(test)]
mod child;
#[cfg(test)]
mod gate;
#[cfg(test)]
#[cfg(feature = "open62541")]
mod peer;

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
        let copy = Path::new("/copy");
        let mut builds = compiler::builds(copy, "-std=c99", "a.c");
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

    /// The names and values of the variables of a build script.
    type Vars<'a> = &'a [(&'a str, &'a str)];

    /// The variables of a build script that has only `vars`.
    fn env(vars: Vars<'_>) -> impl Fn(&str) -> Option<String> {
        move |name| {
            let value = vars.iter().find(|(var, _)| *var == name);
            value.map(|(_, value)| value.to_string())
        }
    }

    const SANITIZE: &str = "CARGO_CFG_SANITIZE";
    const FUZZING: (&str, &str) = ("CARGO_CFG_FUZZING", "");

    /// No public call gives `address_sanitized`: `build.rs` turns it into `cfg(asan)`.
    /// A wrong `true` fails the link of each test binary, but a wrong `false` fails the
    /// `link` test only under `cargo xtask sanitizers`.
    #[test]
    fn sanitize_follows_the_rust_build() {
        let address = [
            "-fsanitize=address,undefined",
            "-fno-sanitize=function",
            "-fno-sanitize-recover=all",
        ];
        let fuzzer = "-fsanitize=fuzzer-no-link";
        let cases: [(Vars<'_>, Vec<&str>); 6] = [
            (&[], vec![]),
            (&[(SANITIZE, "address")], address.to_vec()),
            (&[(SANITIZE, "leak,address")], address.to_vec()),
            (&[(SANITIZE, "leak")], vec![]),
            (&[FUZZING], vec![fuzzer]),
            (
                &[(SANITIZE, "address"), FUZZING],
                [&address[..], &[fuzzer]].concat(),
            ),
        ];
        for (vars, expected) in cases {
            let (mut plain, mut sanitized) =
                (builds("/missing/clang"), builds("/missing/clang"));
            assert_eq!(sanitized.sanitize(env(vars)), Ok(()), "{vars:?}");
            let address_sanitized = expected.contains(&address[0]);
            assert_eq!(sanitized.address_sanitized, address_sanitized, "{vars:?}");
            let pairs = [
                (&mut sanitized.library, &mut plain.library),
                (&mut sanitized.shim, &mut plain.shim),
            ];
            for (build, plain) in pairs {
                let tool = child::tool(build, TARGET);
                assert_eq!(tool.path(), Path::new("/missing/clang"));
                let before = args(&child::tool(plain, TARGET));
                let after = args(&tool);
                assert_eq!(added(&before, &after), expected, "{vars:?}");
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

    /// The error of `sanitize` on the sanitizer `name`.
    fn refusal(name: &str) -> String {
        format!(
            "connector-opcua: the C does not build with the sanitizer `{name}` of the \
             Rust build; it follows only `address` and `leak`"
        )
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
            let vars = [(SANITIZE, sanitize), FUZZING];
            assert_eq!(
                sanitized.sanitize(env(&vars)),
                Err(refusal(name)),
                "{sanitize}"
            );
            assert!(!sanitized.address_sanitized, "{sanitize}");
            let tool = child::tool(&mut sanitized.library, TARGET);
            assert_eq!(tool.path(), Path::new("/missing/gcc"));
            assert_eq!(args(&tool), args(&child::tool(&mut plain.library, TARGET)));
        }
    }

    #[test]
    fn sanitize_moves_gcc_to_clang() {
        let cases: [(Vars<'_>, bool); 2] =
            [(&[(SANITIZE, "address")], true), (&[FUZZING], false)];
        for (vars, address_sanitized) in cases {
            let mut builds = builds("/missing/gcc");
            assert_eq!(builds.sanitize(env(vars)), Ok(()), "{vars:?}");
            assert_eq!(builds.address_sanitized, address_sanitized, "{vars:?}");
            for build in [&mut builds.library, &mut builds.shim] {
                assert_eq!(child::tool(build, TARGET).path(), Path::new("clang"));
            }
        }
    }

    #[test]
    fn sanitize_keeps_gcc_when_the_c_has_no_sanitizer() {
        let cases: [Vars<'_>; 2] = [&[], &[(SANITIZE, "leak")]];
        for vars in cases {
            let mut builds = builds("/missing/gcc");
            assert_eq!(builds.sanitize(env(vars)), Ok(()), "{vars:?}");
            assert!(!builds.address_sanitized, "{vars:?}");
            for build in [&mut builds.library, &mut builds.shim] {
                let path = child::tool(build, TARGET).path().to_owned();
                assert_eq!(path, Path::new("/missing/gcc"), "{vars:?}");
            }
        }
    }

    /// Checks `configure` with a `CC` like MSVC, and does nothing outside a child
    /// process.
    #[test]
    fn configure_in_this_environment() {
        if !child::running() {
            return;
        }
        let configure_in = |vars: Vars<'_>| {
            let vars: Vec<_> = vars
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect();
            compiler::configure(Path::new("/copy"), "", "a.c", &vars)
        };
        let include = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/libcrypto");
        let configure_with = |vars: Vars<'_>| {
            let libcrypto = [("DEP_AWS_LC_0_45_0_INCLUDE", include)];
            configure_in(&[vars, &libcrypto].concat())
        };
        let configure =
            |vars| configure_with(vars).map(|(builds, _)| builds.address_sanitized);
        assert_eq!(
            configure(&[]),
            Err("connector-opcua: the compiler /missing/cl.exe is like MSVC; flags.txt \
                 holds GCC driver flags, which it does not read, so it cannot build \
                 open62541"
                .to_string())
        );
        assert_eq!(configure(&[(SANITIZE, "address")]), Ok(true));
        assert_eq!(configure(&[FUZZING]), Ok(false));
        assert_eq!(configure(&[(SANITIZE, "thread")]), Err(refusal("thread")));
        let server = |vars| {
            let shim = configure_with(vars).unwrap().0.shim;
            let file = shim.get_files().any(|file| file.ends_with("src/server.c"));
            let internal = Path::new("/copy/src").display().to_string();
            let args = args(&tool(shim, "/missing/gcc"));
            (file, includes(&args).contains(&internal.as_str()))
        };
        assert_eq!(server(&[FUZZING]), (false, false));
        assert_eq!(server(&[FUZZING, ("CARGO_FEATURE_SIM", "1")]), (true, true));
        let (builds, libcrypto) = configure_with(&[FUZZING]).unwrap();
        let expected = compiler::Libcrypto {
            include: include.into(),
            prefix: "aws_lc_0_45_0_".to_owned(),
        };
        assert_eq!(libcrypto, expected);
        let library = args(&tool(builds.library, "/missing/gcc"));
        assert_eq!(includes(&library), [include]);
        let missing = |vars: Vars<'_>| configure_in(&[&[FUZZING], vars].concat());
        assert_eq!(
            missing(&[]).map(|(_, libcrypto)| libcrypto),
            Err("connector-opcua: the build script has 0 variables \
                 DEP_AWS_LC_<version>_INCLUDE, not one: []"
                .to_string())
        );
        let header = Path::new("/crypto/openssl/boringssl_prefix_symbols.h");
        let error = std::fs::read_to_string(header).unwrap_err();
        assert_eq!(
            missing(&[("DEP_AWS_LC_0_45_0_INCLUDE", "/crypto")]).map(|(_, l)| l),
            Err(format!("{}: {error}", header.display()))
        );
    }

    #[test]
    fn include_takes_the_one_include_of_aws_lc() {
        let vars = |vars: &[(&str, &str)]| {
            let vars = vars.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned()));
            compiler::include(&vars.collect::<Vec<_>>())
        };
        let include = ("DEP_AWS_LC_0_45_0_INCLUDE", "/a");
        let others = [
            ("DEP_AWS_LC_0_45_0_ROOT", "/b"),
            ("DEP_AWS_LC_FIPS_0_13_0_INCLUDE", "/c"),
            ("DEP_AWS_LC_INCLUDE", "/d"),
            ("DEP_AWS_LC_0_4X_0_INCLUDE", "/e"),
        ];
        assert_eq!(vars(&[others[0], include, others[1]]), Ok("/a".into()));
        let error = |count: usize, found: &str| {
            Err(format!(
                "connector-opcua: the build script has {count} variables \
                 DEP_AWS_LC_<version>_INCLUDE, not one: {found}"
            ))
        };
        assert_eq!(vars(&others), error(0, "[]"));
        let two = ("DEP_AWS_LC_0_46_0_INCLUDE", "/f");
        assert_eq!(
            vars(&[include, two]),
            error(
                2,
                "[(\"DEP_AWS_LC_0_45_0_INCLUDE\", \"/a\"), \
                 (\"DEP_AWS_LC_0_46_0_INCLUDE\", \"/f\")]"
            )
        );
    }

    #[test]
    fn prefix_reads_the_define_of_aws_lc() {
        let header = "#ifndef X\n#define BORINGSSL_PREFIX aws_lc_0_45_0 \n#endif\n";
        assert_eq!(compiler::prefix(header), Ok("aws_lc_0_45_0_".to_owned()));
        let error =
            "connector-opcua: the headers of aws-lc-sys define no BORINGSSL_PREFIX";
        for header in [
            "#define BORINGSSL_PREFIX_SYMBOLS_H\n",
            "#define BORINGSSL_PREFIX \n",
            "#define BORINGSSL_PREFIX\n",
        ] {
            assert_eq!(compiler::prefix(header), Err(error.to_owned()), "{header}");
        }
    }

    /// `configure` checks the compiler that `sanitize` picks, so a sanitizer moves a
    /// `CC` like MSVC to clang, which builds. `build.rs` is the only caller of
    /// `configure`, and a build script has no test harness, so no other test sees the
    /// order of `sanitize` and `check`.
    #[test]
    fn configure_checks_the_compiler_after_sanitize() {
        child::run(
            "tests::configure_in_this_environment",
            &[
                ("CC", "/missing/cl.exe"),
                ("TARGET", TARGET),
                ("HOST", TARGET),
                ("OPT_LEVEL", "0"),
            ],
        );
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
        let flags = "-Ideps\n-Iinclude\n-std=c99";
        let compiler::Builds { library, shim, .. } =
            compiler::builds(copy, flags, "a.c\nsrc/b.c");
        assert_eq!(
            library.get_files().collect::<Vec<_>>(),
            [copy.join("a.c"), copy.join("src/b.c")]
        );
        assert_eq!(
            shim.get_files().collect::<Vec<_>>(),
            [Path::new(env!("CARGO_MANIFEST_DIR")).join("src/shim.c")]
        );
        // `Path::join` puts `\` between the parts on Windows.
        let dirs = ["deps", "include"].map(|dir| copy.join(dir).display().to_string());
        let src = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let alloc = Path::new(src).join("alloc.h");
        let header = format!("-DUA_ARCH_HEADER=\"{}\"", alloc.display());
        for path in ["/missing/gcc", "/missing/clang"] {
            let shim = args(&tool(shim.clone(), path));
            let common = copy.join("arch/common").display().to_string();
            assert_eq!(
                includes(&shim),
                [dirs[0].as_str(), &dirs[1], &common, src],
                "{path}: {shim:?}"
            );
            for arg in ["-Wall", "-Wextra", "-Werror", "-std=c99", &header] {
                assert!(shim.iter().any(|a| a == arg), "{path}: {arg} in {shim:?}");
            }
            let library = args(&tool(library.clone(), path));
            assert!(library.contains(&header), "{path}: {library:?}");
            assert_eq!(
                includes(&library),
                [dirs[0].as_str(), &dirs[1]],
                "{path}: {library:?}"
            );
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
