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
    let (mut library, mut shim) = compiler::builds(&copy, &read("flags.txt"));
    if let Err(e) = compiler::check(&library.get_compiler()) {
        panic!("{e}");
    }
    for source in read("sources.txt").lines() {
        library.file(copy.join(source));
    }
    library.compile("open62541");
    // The copy calls into the shim, so the shim links after it.
    shim.file("src/shim.c").compile("shim");
}

#[cfg(not(feature = "open62541"))]
fn main() {}
