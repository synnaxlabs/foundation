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
    // The tests of `link` compile variants of the shim for this target.
    #[expect(
        clippy::disallowed_methods,
        reason = "cargo gives a build script its target only in the environment"
    )]
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    println!("cargo::rustc-env=CONNECTOR_OPCUA_TARGET={target}");
    let read = |name| {
        let path = copy.join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    };
    let compiler::Builds { library, shim } =
        compiler::builds(&copy, &read("flags.txt"), &read("sources.txt"));
    if let Err(e) = compiler::check(&library.get_compiler()) {
        panic!("{e}");
    }
    library.compile("open62541");
    // The copy calls into the shim, so the shim links after it.
    shim.compile("shim");
}

#[cfg(not(feature = "open62541"))]
fn main() {}
