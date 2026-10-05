//! A library that lifts `disallowed_macros`, which the check refuses.

#![expect(
    clippy::disallowed_macros,
    reason = "a library never holds a global allocator"
)]

#[path = "../moved/mod.rs"]
mod moved;

static mut COUNT: u32 = 0;
pub static TOTAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(crate) static LOCK:
    std::sync::Mutex<u8> = std::sync::Mutex::new(0);
pub(in crate) static FLAG: std::sync::RwLock<bool> = std::sync::RwLock::new(false);
static NAME: &str = "Mutex";
static BYTES: [u8; 4] = [0; 4];
static CELLAR: Cellar = Cellar;

struct Cellar;
