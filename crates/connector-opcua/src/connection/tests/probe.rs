//! Temporary probe of the read path of `State::move_on`. Not for commit.
use std::future::poll_fn;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Instant;

use super::*;

static READS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn count(
    _: *mut ffi::ConnectionManager,
    _: usize,
    _: *mut c_void,
    _: *mut *mut c_void,
    _: ConnectionState,
    _: *const KeyValueMap,
    message: Bytes,
) {
    if message.length > 0 {
        READS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(message.length, Ordering::Relaxed);
        // SAFETY: the manager gives `length` bytes at `data` for the call.
        let first = unsafe { *message.data };
        std::hint::black_box(first);
    }
}

const N: usize = 20_000;

fn stats(name: &str, mut ns: Vec<u64>) {
    ns.sort_unstable();
    let mean = ns.iter().sum::<u64>() as f64 / ns.len() as f64;
    let q = |p: f64| ns[((ns.len() as f64 - 1.0) * p) as usize];
    println!(
        "PROBE {name}: n={} median={} p10={} p90={} p99={} mean={mean:.1} ns",
        ns.len(),
        q(0.5),
        q(0.1),
        q(0.9),
        q(0.99)
    );
}

fn run(bytes: usize) {
    READS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    let mut network = Network::new();
    network.accept(move |mut stream, clock| async move {
        let chunk = vec![7u8; bytes];
        for _ in 0..N + 1000 {
            clock.sleep(Span::MILLISECOND).await;
            let parts = [std::io::IoSlice::new(&chunk)];
            match poll_fn(|cx| stream.poll_write(cx, &parts)).await {
                Ok(_) => {}
                Err(_) => return,
            }
        }
    });
    let remote = network.remote();
    let (read, idle, call) = network
        .sim
        .run_on(&network.local.clone(), move |node, _| async move {
            let mut side = Side::new(&node);
            side.callback = count;
            assert_eq!(side.connect(remote), Status::GOOD);
            side.drive(Span::from_nanos(500_000)).await;
            let clock = node.clock();
            let state = side.manager.state();
            let mut read = Vec::with_capacity(N);
            let mut idle = Vec::with_capacity(N);
            let mut call = Vec::with_capacity(N);
            let mut message = [7u8; 64];
            for _ in 0..N {
                clock.sleep(Span::MILLISECOND).await;
                let (r, i) = poll_fn(|cx| {
                    let s = Instant::now();
                    state.move_on(1, cx);
                    let r = s.elapsed();
                    let s = Instant::now();
                    state.move_on(1, cx);
                    Poll::Ready((r, s.elapsed()))
                })
                .await;
                read.push(r.as_nanos() as u64);
                let s = Instant::now();
                state.call(1, ffi::ESTABLISHED, &mut message);
                call.push(s.elapsed().as_nanos() as u64);
                idle.push(i.as_nanos() as u64);
            }
            (read, idle, call)
        })
        .expect("the run ends");
    println!("PROBE samples={}", read.len());
    println!(
        "PROBE bytes={bytes}: reads={} bytes read={}",
        READS.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed)
    );
    stats(&format!("move_on with a read of {bytes} B"), read);
    stats(&format!("move_on with no read ({bytes} B run)"), idle);
    stats(&format!("unchanged call of 64 B ({bytes} B run)"), call);
}

#[test]
#[ignore = "probe"]
fn probe_read_path() {
    for _ in 0..3 {
        for bytes in [64, 1024, 8192] {
            run(bytes);
        }
    }
}
