use std::path::{Path, PathBuf};

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

/// The directory of our C.
const SRC: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src");

/// The two builds of `build.rs`, and whether they build with the address sanitizer.
pub(crate) struct Builds {
    /// The open62541 copy, with its warnings off.
    pub(crate) library: Build,
    /// `src/shim.c`, and, from [`configure`] with `CARGO_FEATURE_SIM`, the test server
    /// `src/server.c`: our code, so its warnings are errors.
    pub(crate) shim: Build,
    /// Whether the C builds with the address sanitizer.
    pub(crate) address_sanitized: bool,
}

/// The libcrypto of `aws-lc-sys` that the OpenSSL plugins of the copy call.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Libcrypto {
    /// Its include directory.
    pub(crate) include: PathBuf,
    /// The prefix of each of its symbols, with the `_` that ends it.
    pub(crate) prefix: String,
}

/// Gives the libcrypto of `aws-lc-sys` of `vars`, the variables of a build script: its
/// [`include`] directory, and the [`prefix`] of the header
/// `openssl/boringssl_prefix_symbols.h` in it.
///
/// # Errors
///
/// The error of [`include`] or of [`prefix`], or the path of the header and its error
/// when it does not read.
pub(crate) fn libcrypto(vars: &[(String, String)]) -> Result<Libcrypto, String> {
    let include = include(vars)?;
    let header = include.join("openssl/boringssl_prefix_symbols.h");
    let text = std::fs::read_to_string(&header)
        .map_err(|e| format!("{}: {e}", header.display()))?;
    Ok(Libcrypto {
        prefix: prefix(&text)?,
        include,
    })
}

/// Gives the include directory of the libcrypto of `aws-lc-sys`, from `vars`, the
/// variables of a build script. Cargo gives a build script the `links` metadata of each
/// direct dependency, and the name of the `links` of `aws-lc-sys` holds its version.
///
/// # Errors
///
/// When `vars` holds no variable `DEP_AWS_LC_<version>_INCLUDE`, or more than one.
pub(crate) fn include(vars: &[(String, String)]) -> Result<PathBuf, String> {
    let found: Vec<_> = vars
        .iter()
        .filter(|(name, _)| {
            name.strip_prefix("DEP_AWS_LC_")
                .and_then(|rest| rest.strip_suffix("_INCLUDE"))
                .is_some_and(|version| {
                    version.split('_').all(|n| n.parse::<u32>().is_ok())
                })
        })
        .collect();
    match <[_; 1]>::try_from(found) {
        Ok([(_, dir)]) => Ok(PathBuf::from(dir)),
        Err(found) => Err(format!(
            "connector-opcua: the build script has {} variables \
             DEP_AWS_LC_<version>_INCLUDE, not one: {found:?}",
            found.len()
        )),
    }
}

/// Gives the prefix of each symbol of the libcrypto of `aws-lc-sys`, with the `_` that
/// ends it, from `header`, the text of its `openssl/boringssl_prefix_symbols.h`.
///
/// # Errors
///
/// When `header` has no line `#define BORINGSSL_PREFIX <prefix>` with a `<prefix>`
/// that is not empty.
pub(crate) fn prefix(header: &str) -> Result<String, String> {
    let prefix = header
        .lines()
        .find_map(|line| line.strip_prefix("#define BORINGSSL_PREFIX "))
        .map(str::trim)
        .filter(|prefix| !prefix.is_empty());
    prefix.map(|prefix| format!("{prefix}_")).ok_or_else(|| {
        "connector-opcua: the headers of aws-lc-sys define no BORINGSSL_PREFIX"
            .to_owned()
    })
}

/// Gives the builds of [`builds`] under the sanitizers of the Rust build, as
/// [`Builds::sanitize`] does, and the [`libcrypto`] of `vars`, whose include directory
/// is on the include path of the library only. `vars` holds the variables of a build
/// script, and the shim build holds `src/server.c`, with the `src` folder of the copy
/// on its include path, when they set `CARGO_FEATURE_SIM`.
///
/// # Errors
///
/// The error of [`libcrypto`], of [`Builds::sanitize`], or of [`check`] on the
/// compiler that it picks.
pub(crate) fn configure(
    copy: &Path,
    flags: &str,
    sources: &str,
    vars: &[(String, String)],
) -> Result<(Builds, Libcrypto), String> {
    let env = |name: &str| {
        let var = vars.iter().find(|(var, _)| var == name);
        var.map(|(_, value)| value.clone())
    };
    let libcrypto = libcrypto(vars)?;
    let mut builds = builds(copy, flags, sources);
    // Only the OpenSSL plugins of the copy include the headers of the libcrypto.
    builds.library.include(&libcrypto.include);
    if env("CARGO_FEATURE_SIM").is_some() {
        sim(&mut builds.shim, copy);
    }
    builds.sanitize(env)?;
    // After `sanitize`, which can replace the compiler with clang.
    check(&builds.library.get_compiler())?;
    Ok((builds, libcrypto))
}

/// Adds the test server `src/server.c` to `shim`, the shim build of the copy at
/// `copy`.
pub(crate) fn sim(shim: &mut Build, copy: &Path) {
    shim.file(Path::new(SRC).join("server.c"));
    // The test server reads the continuation points of a session, which have no public
    // header.
    shim.include(copy.join("src"));
}

/// Gives the builds of the open62541 copy at `copy` and of `src/shim.c`
/// ([`Builds::shim`]). `flags` is the text of the copy's `flags.txt`, whose include
/// paths are relative to the copy. `sources` is the text of `sources.txt`, one file of
/// the copy on each line.
pub(crate) fn builds(copy: &Path, flags: &str, sources: &str) -> Builds {
    let mut library = Build::new();
    library.warnings(false);
    let mut shim = Build::new();
    shim.warnings(true).warnings_into_errors(true);
    let header = format!("\"{}\"", Path::new(SRC).join("alloc.h").display());
    library.define("UA_ARCH_HEADER", header.as_str());
    shim.define("UA_ARCH_HEADER", header.as_str());
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
    // `shim.c` builds its event loop on the copy's timer, which has no public header.
    shim.include(copy.join("arch/common"));
    // A test compiles `shim.c` from its standard input, so `shim.h` is on the path.
    shim.include(SRC);
    for source in sources.lines() {
        library.file(copy.join(source));
    }
    shim.file(Path::new(SRC).join("shim.c"));
    Builds {
        library,
        shim,
        address_sanitized: false,
    }
}

impl Builds {
    /// Gives both builds the sanitizers of the Rust build, and sets
    /// `address_sanitized`. `env` gives the variables of a build script. When
    /// `cfg(sanitize)` holds `address`, the C runs under the address and undefined
    /// behavior sanitizers, and stops at the first error. Under `cfg(fuzzing)`, it
    /// gives libFuzzer its coverage.
    ///
    /// # Errors
    ///
    /// A sanitizer other than `address` and `leak`, which the C does not follow, and
    /// which reports false errors or misses the C without it.
    pub(crate) fn sanitize(
        &mut self,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<(), String> {
        let sanitize = env("CARGO_CFG_SANITIZE").unwrap_or_default();
        let mut names = sanitize.split(',').filter(|name| !name.is_empty());
        if let Some(name) = names
            .clone()
            .find(|name| !matches!(*name, "address" | "leak"))
        {
            return Err(format!(
                "connector-opcua: the C does not build with the sanitizer `{name}` of \
                 the Rust build; it follows only `address` and `leak`"
            ));
        }
        self.address_sanitized = names.any(|name| name == "address");
        if self.address_sanitized {
            // `ZIP_FUNCTIONS` of the copy calls each comparator through a generic
            // function type, which `-fsanitize=function` stops on.
            self.add(&[
                "-fsanitize=address,undefined",
                "-fno-sanitize=function",
                "-fno-sanitize-recover=all",
            ]);
        }
        if env("CARGO_CFG_FUZZING").is_some() {
            self.add(&["-fsanitize=fuzzer-no-link"]);
        }
        Ok(())
    }

    /// Adds `flags` to both builds. A compiler that is not clang gives way to `clang`,
    /// since rustc links the LLVM runtimes of the sanitizers.
    fn add(&mut self, flags: &[&str]) {
        let clang = self.library.get_compiler().is_like_clang();
        for build in [&mut self.library, &mut self.shim] {
            if !clang {
                build.compiler("clang");
            }
            for flag in flags {
                build.flag(flag);
            }
        }
    }
}
