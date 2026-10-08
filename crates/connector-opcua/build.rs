//! Compiles the open62541 copy in `patches/open62541/` and `src/shim.c`, with the
//! feature `open62541`.

#[cfg(feature = "open62541")]
#[path = "build/compiler.rs"]
mod compiler;

#[cfg(feature = "open62541")]
fn main() {
    use std::path::Path;

    let copy = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../patches/open62541");
    println!("cargo::rerun-if-changed={}", copy.display());
    println!("cargo::rerun-if-changed=src/shim.c");
    let read = |name| {
        let path = copy.join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    };
    let mut build = cc::Build::new();
    if let Err(e) = compiler::check(&build.get_compiler()) {
        panic!("{e}");
    }
    // The shim is our code, so its warnings are errors.
    let mut shim = cc::Build::new();
    shim.warnings(true)
        .extra_warnings(true)
        .warnings_into_errors(true);
    for flag in read("flags.txt").lines() {
        // Each include path is relative to the copy.
        if let Some(dir) = flag.strip_prefix("-I") {
            let dir = copy.join(dir);
            build.include(&dir);
            shim.include(dir);
        } else {
            build.flag(flag);
            shim.flag(flag);
        }
    }
    for source in read("sources.txt").lines() {
        build.file(copy.join(source));
    }
    build.warnings(false).compile("open62541");
    // The copy calls into the shim, so the shim links after it.
    shim.file("src/shim.c").compile("shim");
}

#[cfg(not(feature = "open62541"))]
fn main() {}
