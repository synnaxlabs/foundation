//! Probe of the `kind` lines for #3556, not for merge. Arguments: MODE NODES R N, where
//! MODE is `drive` (drive NODES NODES 1: a spread of 1 ns), or `write`, `one`, or
//! `half` (writes of NODES f64 with bad 0, 1, or 1/2). R runs of N each. Each run of
//! `drive` prints its p50 in ns to stderr.
#![allow(clippy::all)]
use std::time::Instant;
use types::sample::Scalar;
use types::time::Span;
#[path = "../tests/common/kind.rs"]
mod kind;
fn time(work: &mut dyn FnMut()) -> u64 {
    let start = Instant::now();
    work();
    Instant::now().duration_since(start).as_nanos() as u64
}
fn main() {
    let args: Vec<String> = std::env::args().skip(1).filter(|a| !a.starts_with("--")).collect();
    let mode = args[0].as_str();
    let nodes: usize = args[1].parse().unwrap();
    let r: usize = args[2].parse().unwrap();
    let n: usize = args[3].parse().unwrap();
    let mut s = 0;
    for _ in 0..r {
        if mode == "drive" {
            let one = Span::from_nanos(1);
            let mut m = kind::drives(nodes, &[Scalar::F64], n, nodes, one, time);
            m.sort_unstable();
            eprintln!("p50 {}", m[m.len() / 2]);
            s += m.len();
        } else {
            let bad: [usize; 2] = match mode {
                "one" => [1, 1],
                "half" => [1, 2],
                _ => [0, 0],
            };
            let w = kind::writes(nodes, &[Scalar::F64], n, move |w| bad[w % 2], time);
            s += w.0.len();
        }
    }
    println!("{s}");
}
