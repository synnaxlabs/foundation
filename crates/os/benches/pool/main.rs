//! A `block` pool over OS memory under pressure: the system calls of each alloc and
//! the first write of each page it gives back.

#[cfg(any(target_os = "linux", target_os = "macos"))] // #477
mod unix;

fn main() {
    divan::main();
}
