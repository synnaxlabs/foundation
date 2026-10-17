//! Carry records under a steady load of many paths.

use super::*;

/// `paths` live paths get one entry each in turn, four in each commit, for 600
/// commits on a ring of 24 blocks with trims: with `empty`, an entry with no
/// samples at seq 0, else one sample at the next seq. Gives the first commit that
/// the ring still refuses after 1000 commit spans, with its error.
fn refused_with_paths(seed: u64, paths: u32, empty: bool) -> Option<(u64, Rejected)> {
    let (mut sim, node) = create_node(seed);
    let own = node.clone();
    let found = Arc::new(Mutex::new(None));
    let given = Arc::clone(&found);
    drop(on_node(&node, "stall", move |tasks| async move {
        let config = Config {
            layout: layout(24 * BLOCK, BODY_MAX),
            ..node_config(&own, tasks, DIR)
        };
        let clock = own.clock();
        let mut slots = Slots::new();
        let buffer = Buffer::open(config, &mut slots).await.expect("opens");
        let mut next = 0;
        for commit in 0..600 {
            let batch: Vec<Entry> = (0..4)
                .map(|_| {
                    let index = next % paths + 1;
                    let lap = u64::from(next / paths);
                    next += 1;
                    let slot = slots.index(key(index));
                    if empty {
                        entry(index, slot, Path::Live, 0, 0, None, Parts::default())
                    } else {
                        let last = i64::try_from(lap).expect("small") + 1;
                        let parts = Parts::default();
                        entry(index, slot, Path::Live, lap, 1, Some(last), parts)
                    }
                })
                .collect();
            if let Err(error) = append_within(&buffer, &clock, &batch, 1000).await {
                *given.lock().expect("no panic held the lock") = Some((commit, error));
                return;
            }
            buffer.committed().await.expect("commits");
        }
    }));
    sim.run_for(Span::from_nanos(600_000_000_000))
        .expect("the run goes on");
    found.lock().expect("no panic held the lock").take()
}

/// A carry record that holds one old path takes newer paths up to its limit,
/// so that 1000 paths, more than ten records of tails, never stall the ring.
#[test]
fn a_thousand_paths_with_samples_do_not_stall_the_ring() {
    let failures: Vec<_> = (0..32)
        .filter_map(|seed| refused_with_paths(seed, 1000, false).map(|r| (seed, r)))
        .collect();
    assert_eq!(failures, Vec::new());
}

/// Paths of entries with no samples at one seq keep their counts in their headers,
/// so a trim may land between any two records, and 292 such paths, more tails than
/// three free blocks hold, never stall the ring.
#[test]
fn paths_of_entries_with_no_samples_do_not_stall_the_ring() {
    let failures: Vec<_> = (0..32)
        .filter_map(|seed| refused_with_paths(seed, 292, true).map(|r| (seed, r)))
        .collect();
    assert_eq!(failures, Vec::new());
}

/// The other cases of #2796: 292 and 600 paths with samples, and 291 paths of
/// entries with no samples, never stall the ring.
#[test]
fn paths_near_the_room_of_a_carry_record_do_not_stall_the_ring() {
    let cases = [(292, false), (600, false), (291, true)];
    let failures: Vec<_> = cases
        .into_iter()
        .flat_map(|(paths, empty)| (0..32).map(move |seed| (seed, paths, empty)))
        .filter_map(|(seed, paths, empty)| {
            refused_with_paths(seed, paths, empty).map(|r| (seed, paths, r))
        })
        .collect();
    assert_eq!(failures, Vec::new());
}

/// A path whose newest record holds only an entry with no samples keeps that record
/// as its holder through the trims that pass its older record, so a reopen counts
/// on from both entries.
#[test]
fn a_reopen_after_trims_counts_on_from_the_holder_of_entries_with_no_samples() {
    let (mut sim, node) = create_node(0);
    sim.run_on(&node, |node, tasks| async move {
        let config = || Config {
            layout: layout(32 * BLOCK, BODY_MAX),
            ..node_config(&node, tasks.clone(), DIR)
        };
        let first = config();
        let pool = Rc::clone(&first.pool);
        let clock = node.clock();
        let mut slots = Slots::new();
        let buffer = Buffer::open(first, &mut slots).await.expect("opens");
        let one = slots.index(key(1));
        let two = slots.index(key(2));
        let empty = || entry(1, one, Path::Live, 0, 0, None, Parts::default());
        for _ in 0..2 {
            buffer.append([empty()]).expect("the ring has room");
            buffer.committed().await.expect("commits");
        }
        for seq in 0..200 {
            let parts = Parts::from(pool.alloc(100).expect("a block").freeze());
            let last = i64::try_from(seq).expect("a small seq") + 1;
            let data = entry(2, two, Path::Live, seq, 1, Some(last), parts);
            append_within(&buffer, &clock, &[data], 100)
                .await
                .expect("the ring frees room");
            buffer.committed().await.expect("commits");
        }
        let ending = buffer.ended();
        drop(buffer);
        ending.await.expect("ends");
        let mut slots = Slots::new();
        let buffer = Buffer::open(config(), &mut slots).await.expect("reopens");
        let one = slots.index(key(1));
        let empty = entry(1, one, Path::Live, 0, 0, None, Parts::default());
        buffer.append([empty]).expect("the ring has room");
        buffer.committed().await.expect("commits");
        let read = buffer
            .reader()
            .read(one, Path::Live, mark(0, 0), usize::MAX)
            .await
            .expect("reads");
        assert_eq!(read.next, mark(0, 3));
    })
    .expect("the buffer ends");
}

/// Paths 1 to 8 write once, then 400 commits of path 9 in records of 4 blocks turn
/// a ring of 512 blocks three times, so each of paths 1 to 8 is carried. The trims
/// write at most one checkpoint for each step of 100 blocks of records, and the
/// open writes one more.
#[test]
fn a_full_ring_writes_one_checkpoint_for_each_step() {
    let (mut sim, node) = create_node(0);
    let (seqs, durables) = sim
        .run_on(&node, |node, tasks| async move {
            let config = || Config {
                layout: layout(512 * BLOCK, 4 * 4096 - 9),
                ..node_config(&node, tasks.clone(), DIR)
            };
            let first = config();
            let pool = Rc::clone(&first.pool);
            let clock = node.clock();
            let mut slots = Slots::new();
            let buffer = Buffer::open(first, &mut slots).await.expect("opens");
            let once: Vec<Entry> = (1..=8)
                .map(|index| {
                    let slot = slots.index(key(index));
                    entry(index, slot, Path::Live, 0, 1, Some(1), Parts::default())
                })
                .collect();
            buffer.append(once).expect("the ring has room");
            buffer.committed().await.expect("commits");
            let nine = slots.index(key(9));
            let part = pool.alloc(3 * 4096).expect("a block").freeze();
            for seq in 0..400 {
                let last = i64::try_from(seq).expect("a small seq") + 1;
                let parts = Parts::from(part.clone());
                let data = entry(9, nine, Path::Live, seq, 1, Some(last), parts);
                append_within(&buffer, &clock, &[data], 100)
                    .await
                    .expect("the ring frees room");
                buffer.committed().await.expect("commits");
            }
            let ending = buffer.ended();
            drop(buffer);
            ending.await.expect("ends");
            let ring = node.files().open(FilePath::new(RING), Mode::Read).await;
            let ring = ring.expect("the ring opens");
            let mut seqs = Vec::new();
            for at in [0, BLOCK] {
                let block = pool.alloc(to_usize(BLOCK)).expect("a block");
                let header = ring.read_at(at, block).await.expect("reads");
                let seq = header[SEQ_AT..SEQ_AT + 8].try_into().expect("8 bytes");
                seqs.push(u64::from_le_bytes(seq));
            }
            drop(ring);
            let mut slots = Slots::new();
            let buffer = Buffer::open(config(), &mut slots).await.expect("reopens");
            let durables: Vec<u64> = (1..=8)
                .map(|index| buffer.durable(slots.index(key(index)), Path::Live).seq)
                .collect();
            let ending = buffer.ended();
            drop(buffer);
            ending.await.expect("ends");
            (seqs, durables)
        })
        .expect("the buffer ends");
    assert_eq!(durables, vec![1; 8], "each path is carried");
    let checkpoints = seqs.into_iter().max().expect("two blocks") + 1;
    let blocks = 1 + 400 * 4;
    assert!(checkpoints <= blocks / 100 + 1, "{checkpoints} checkpoints");
}
