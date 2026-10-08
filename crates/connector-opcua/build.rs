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
    for flag in read("flags.txt").lines() {
        // Each include path is relative to the copy.
        match flag.strip_prefix("-I") {
            Some(dir) => build.include(copy.join(dir)),
            None => build.flag(flag),
        };
    }
    for source in read("sources.txt").lines() {
        build.file(copy.join(source));
    }
    build
        .file("src/shim.c")
        .warnings(false)
        .compile("open62541");
}

#[cfg(not(feature = "open62541"))]
fn main() {}
