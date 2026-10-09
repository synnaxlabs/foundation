use std::path::Path;

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

/// The two builds of `build.rs`.
pub(crate) struct Builds {
    /// The open62541 copy, with its warnings off.
    pub(crate) library: Build,
    /// `src/shim.c`, our code, so its warnings are errors.
    pub(crate) shim: Build,
}

/// Gives the builds of the open62541 copy at `copy` and of `src/shim.c`. `flags` is
/// the text of the copy's `flags.txt`, whose include paths are relative to the copy.
/// `sources` is the text of `sources.txt`, one file of the copy on each line.
pub(crate) fn builds(copy: &Path, flags: &str, sources: &str) -> Builds {
    let mut library = Build::new();
    library.warnings(false);
    let mut shim = Build::new();
    shim.warnings(true).warnings_into_errors(true);
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let header = format!("\"{}\"", root.join("src/alloc.h").display());
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
    for source in sources.lines() {
        library.file(copy.join(source));
    }
    shim.file(root.join("src/shim.c"));
    Builds { library, shim }
}

impl Builds {
    /// Gives both builds the sanitizers of the Rust build, and returns whether the C
    /// builds with the address sanitizer. `env` gives the variables of a build script.
    /// When `cfg(sanitize)` holds `address`, the C runs under the address and undefined
    /// behavior sanitizers, and stops at the first error. Under `cfg(fuzzing)`, it
    /// gives libFuzzer its coverage.
    ///
    /// # Panics
    ///
    /// On a sanitizer other than `address` and `leak`, which the C does not follow, and
    /// which reports false errors or misses the C without it.
    pub(crate) fn sanitize(&mut self, env: impl Fn(&str) -> Option<String>) -> bool {
        let sanitize = env("CARGO_CFG_SANITIZE").unwrap_or_default();
        let mut names = sanitize.split(',').filter(|name| !name.is_empty());
        if let Some(name) = names
            .clone()
            .find(|name| !matches!(*name, "address" | "leak"))
        {
            panic!(
                "connector-opcua: the C does not build with the sanitizer `{name}` of \
                 the Rust build; it follows only `address` and `leak`"
            );
        }
        let address = names.any(|name| name == "address");
        if address {
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
        address
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
