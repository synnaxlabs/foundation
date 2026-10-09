//! Compiles the open62541 copy in `patches/open62541/` and `src/shim.c`, with the
//! feature `open62541`, under the sanitizers of the Rust build.

#[cfg(feature = "open62541")]
#[path = "build/compiler.rs"]
mod compiler;

fn main() {
    // The tests compile C for this target.
    #[expect(
        clippy::disallowed_methods,
        reason = "cargo gives a build script its target only in the environment"
    )]
    let target = std::env::var("TARGET").expect("cargo sets TARGET");
    println!("cargo::rustc-env=CONNECTOR_OPCUA_TARGET={target}");
    // Set when C is built with ASan: `src/alloc.rs` then poisons the header of each
    // block.
    println!("cargo::rustc-check-cfg=cfg(asan)");
    #[cfg(feature = "open62541")]
    build();
}

#[cfg(feature = "open62541")]
fn build() {
    use std::path::Path;

    let copy = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../patches/open62541");
    println!("cargo::rerun-if-changed={}", copy.display());
    println!("cargo::rerun-if-changed=src/shim.c");
    println!("cargo::rerun-if-changed=src/alloc.h");
    println!("cargo::rerun-if-changed={}", compiler::PROBE);
    let read = |name| {
        let path = copy.join(name);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    };
    let mut builds = compiler::builds(&copy, &read("flags.txt"), &read("sources.txt"));
    #[expect(
        clippy::disallowed_methods,
        reason = "cargo gives a build script the cfgs of its target only in the \
                  environment"
    )]
    let (sanitize, fuzzing) = (
        std::env::var("CARGO_CFG_SANITIZE").unwrap_or_default(),
        std::env::var_os("CARGO_CFG_FUZZING").is_some(),
    );
    builds.sanitize(&compiler::sanitizers(&sanitize, fuzzing));
    let compiler::Builds { mut library, shim } = builds;
    let tool = library.get_compiler();
    match compiler::check(&tool).and_then(|()| compiler::asan(&tool)) {
        Ok(true) => println!("cargo::rustc-cfg=asan"),
        Ok(false) => {}
        Err(e) => panic!("{e}"),
    }
    // The copy and the shim call each other, so they share one archive: a linker that
    // reads each archive once, such as GNU ld, finds no order of two that links.
    library.objects(shim.compile_intermediates());
    library.compile("open62541");
}
