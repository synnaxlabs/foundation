//! Reads, subscribes to, and writes OPC UA servers through open62541, compiled in with
//! the feature `open62541`.

#[cfg(test)]
#[cfg(feature = "open62541")]
mod link;

// `build.rs` calls it, and a build script has no test harness.
#[cfg(test)]
#[path = "../build/compiler.rs"]
mod compiler;

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::compiler;

    #[test]
    fn check_refuses_a_compiler_that_does_not_read_gcc_flags() {
        assert_eq!(
            compiler::check(Path::new("cl.exe"), false),
            Err(
                "connector-opcua: the compiler cl.exe is not GCC or clang; flags.txt \
                 holds GCC driver flags, so only GCC and clang can build open62541"
                    .to_owned()
            )
        );
        assert_eq!(compiler::check(Path::new("cc"), true), Ok(()));
    }
}
