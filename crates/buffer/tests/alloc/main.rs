//! The ring's append and recovery make no heap allocation. This binary has no test
//! harness: the count covers each thread, and a harness allocates on its own thread
//! at any time.

use buffer::bench::Ring;

#[global_allocator]
static ALLOCATOR: counting::Allocator = counting::Allocator::new();

fn main() {
    let bodies: Vec<Vec<u8>> = [0usize, 64, 4087, 1 << 16]
        .into_iter()
        .map(|len| vec![0xA5; len])
        .collect();
    let mut ring = Ring::new(1 << 22, 1 << 16);
    let ((), appends) = ALLOCATOR.count(|| {
        for body in bodies.iter().cycle() {
            if !ring.write(body) {
                break;
            }
        }
    });
    assert!(ring.records() > 100, "{} records fit", ring.records());
    assert_eq!(appends, 0, "appends allocate");

    let (found, walk) = ALLOCATOR.count(|| ring.walk());
    assert_eq!(found, ring.records(), "the walk finds every record");
    assert_eq!(walk, 0, "the walk allocates");
}
