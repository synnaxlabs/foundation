//! Tests of the read of the spec in use that gets a missing chunk from a peer, on a
//! cluster of three nodes.

use super::apply::{assert_used, base, create_subjects, pointer};
use super::*;
use crate::driver::peer::DEADLINE;
use crate::used::Cause;

// Voter 3 is down at the apply, so it holds no chunk of the change.
#[test]
fn a_voter_down_at_the_apply_uses_the_spec_when_it_comes_back() {
    let mut cluster = Cluster::new(0);
    cluster.start_voter(1);
    cluster.start_voter(2);
    cluster.run(seconds(5));
    let definitions = create_subjects(&["plant.a"], 1);
    cluster.change(1, base(), &definitions);
    cluster.run(seconds(5));
    cluster.start_voter(3);
    cluster.run(seconds(5));
    assert_used(
        &cluster.board(),
        &IDS,
        pointer(1, &definitions),
        &definitions,
    );
}

// Voter 3 is down at the apply of a change of 40 subjects, so it lacks each of its 41
// listed chunks. Voter 2 takes three puts at the apply, and voter 3 gets the chunks
// from voter 1, the first voter, in three calls of `CHUNKS`, one stream each.
#[test]
fn a_voter_that_lacks_many_chunks_gets_them_in_calls_of_chunks() {
    let (mut cluster, definitions) = lacking(0);
    cluster.start_voter(3);
    cluster.run(seconds(10));
    let board = cluster.board();
    assert_used(&board, &[3], pointer(1, &definitions), &definitions);
    assert_eq!(board.blobs, [(1, 3), (2, 3)].into());
}

// As above, but voter 1 loses power before voter 3 starts. Voter 3 asks voter 1 once,
// and voter 2 gives each chunk.
#[test]
fn a_voter_down_first_costs_the_get_once() {
    for seed in 0..4 {
        let (mut cluster, definitions) = lacking(seed);
        let one = cluster.node(1).clone();
        cluster.sim.crash(&one, Crash::Power);
        cluster.start_voter(3);
        cluster.run(seconds(60));
        let seen = cluster.board().specs[&3].pointer;
        assert_eq!(seen, Some(pointer(1, &definitions)), "seed {seed}");
    }
}

// Voter 1 holds each blob stream with no answer. A call of `CHUNKS` to it that passes
// the deadline gives each of the three calls to voter 2, so the get costs the
// deadline once, not once for each call.
#[test]
fn a_silent_first_voter_costs_the_deadline_once_for_all_calls() {
    let (mut cluster, definitions) = lacking(0);
    cluster.board.lock().unwrap().silent = [1].into();
    cluster.start_voter(3);
    cluster.run(Span::from_nanos(2 * DEADLINE.nanos() - TICK.nanos()));
    let board = cluster.board();
    assert_used(&board, &[3], pointer(1, &definitions), &definitions);
    assert_eq!(board.blobs, [(1, 1), (2, 6)].into());
}

// Voter 3 is down at two changes. The second lists only the chunks that it adds, so
// the read of voter 3 finds each chunk of the first one at a time. Voter 2 gives the
// listed chunks after voter 1 passes the deadline, and each later get starts at
// voter 2.
#[test]
fn a_get_starts_at_the_voter_that_gave_the_last_chunk() {
    let (mut cluster, first) = lacking(0);
    let mut definitions = first.clone();
    definitions.extend(create_subjects(&["plant.z"], 1));
    cluster.change(1, pointer(1, &first), &definitions);
    cluster.run(seconds(10));
    cluster.board.lock().unwrap().silent = [1].into();
    cluster.start_voter(3);
    cluster.run(seconds(30));
    assert_used(
        &cluster.board(),
        &[3],
        pointer(2, &definitions),
        &definitions,
    );
}

// Voter 3 loses the root of the spec in use while it is down.
#[test]
fn a_voter_that_lost_a_chunk_gets_it_from_another_voter() {
    let (mut cluster, definitions) = applied();
    let root = pointer(1, &definitions).root;
    restart(&mut cluster, move |node| async move {
        let path = Path::new(BLOB).join(root.to_string());
        node.files().remove(&path).await.unwrap();
    });
    cluster.run(seconds(5));
    assert_used(
        &cluster.board(),
        &[3],
        pointer(1, &definitions),
        &definitions,
    );
}

// Voter 1, a holder and the first peer of voter 3, loses the root while it runs, so
// it answers the get of voter 3 with a miss.
#[test]
fn a_peer_that_lacks_the_chunk_leaves_the_get_to_the_next_peer() {
    let (mut cluster, definitions) = applied();
    let root = pointer(1, &definitions).root;
    let path = Path::new(BLOB).join(root.to_string());
    let node = cluster.node(1).clone();
    let config = env::shards::Config {
        name: "remove".into(),
        core: None,
    };
    let (own, at) = (node.clone(), path.clone());
    let remove = move |_| async move { own.files().remove(&at).await.unwrap() };
    drop(node.shards().start(config, remove).unwrap());
    restart(&mut cluster, move |node| async move {
        node.files().remove(&path).await.unwrap();
    });
    cluster.run(seconds(5));
    assert_used(
        &cluster.board(),
        &[3],
        pointer(1, &definitions),
        &definitions,
    );
}

#[test]
fn a_voter_that_restarts_uses_the_spec_with_no_get() {
    let (mut cluster, definitions) = applied();
    restart(&mut cluster, |_| async {});
    cluster.run(seconds(5));
    let board = cluster.board();
    assert_used(&board, &[3], pointer(1, &definitions), &definitions);
    assert_eq!(board.blobs, BTreeMap::new());
}

// Voter 3 is down at the apply, so the holders are 1 and 2. Voter 1 then holds each
// blob stream with no answer, so voter 3 asks it first and gets each chunk from
// voter 2 after the deadline.
#[test]
fn a_holder_that_does_not_answer_leaves_the_get_to_the_next_peer() {
    let mut cluster = Cluster::new(0);
    cluster.start_voter(1);
    cluster.start_voter(2);
    cluster.run(seconds(5));
    let definitions = create_subjects(&["plant.a"], 1);
    cluster.change(1, base(), &definitions);
    cluster.run(seconds(5));
    cluster.board.lock().unwrap().silent = [1].into();
    cluster.start_voter(3);
    cluster.run(seconds(5));
    let moved = pointer(1, &definitions);
    let seen = cluster.board.lock().unwrap().specs[&3].pointer;
    assert_eq!(seen, Some(base()));
    cluster.run(seconds(30));
    assert_used(&cluster.board(), &[3], moved, &definitions);
}

// Voter 3 lost the root of the spec in use, and its store fails the put of the root
// that voter 1 gives.
#[test]
fn a_failed_put_of_a_got_chunk_gives_the_error_of_the_store_until_a_retry() {
    let (mut cluster, definitions) = applied();
    let moved = pointer(1, &definitions);
    restart(&mut cluster, move |node| async move {
        let path = Path::new(BLOB).join(moved.root.to_string());
        node.files().remove(&path).await.unwrap();
        node.fail_file(Path::new(BLOB), Operation::SyncDir);
    });
    let cause = files::Error::Io {
        path: BLOB.into(),
        operation: Operation::SyncDir,
        code: 5,
    };
    let failed = Behind {
        pointer: moved,
        cause: Cause::Blob(blob::Error::Files(cause)),
    };
    let mut seen = Vec::new();
    for _ in 0..50 {
        cluster.run(TICK);
        let board = cluster.board.lock().unwrap();
        seen.extend(board.specs.get(&3).and_then(|seen| seen.behind.clone()));
    }
    assert!(seen.contains(&failed), "{seen:?}");
    assert_used(&cluster.board(), &[3], moved, &definitions);
}

// A cluster of three voters, each of which uses the spec of a change that voter 1
// applied, and the definitions of that spec.
fn applied() -> (Cluster, BTreeMap<Name, Definition>) {
    let mut cluster = Cluster::new(0);
    cluster.start();
    cluster.run(seconds(5));
    let definitions = create_subjects(&["plant.a"], 1);
    cluster.change(1, base(), &definitions);
    cluster.run(seconds(5));
    assert_used(
        &cluster.board(),
        &IDS,
        pointer(1, &definitions),
        &definitions,
    );
    (cluster, definitions)
}

// A cluster in which voters 1 and 2 use a change of 40 subjects that voter 3, which
// has not started, lacks, and the definitions of that change.
fn lacking(seed: u64) -> (Cluster, BTreeMap<Name, Definition>) {
    let mut cluster = Cluster::new(seed);
    cluster.start_voter(1);
    cluster.start_voter(2);
    cluster.run(seconds(5));
    let labels: Vec<String> = (0..40).map(|at| format!("plant.s{at}")).collect();
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    let definitions = create_subjects(&labels, 512);
    cluster.change(1, base(), &definitions);
    cluster.run(seconds(10));
    (cluster, definitions)
}

// Voter 1 is silent and voter 3 lacks the chunks of the first change. Voter 2 then
// applies a change each 8 s, which replaces each pointer before `DEADLINE` ends. A get
// that a newer pointer drops at voter 1 makes the next get start at voter 2.
#[test]
fn a_change_each_8_s_with_a_silent_first_voter_keeps_no_voter_behind() {
    let mut behind = Vec::new();
    for seed in 0..32 {
        let (mut cluster, first) = lacking(seed);
        cluster.board.lock().unwrap().silent = [1].into();
        cluster.start_voter(3);
        cluster.run(seconds(1));
        let mut definitions = first.clone();
        let mut base = pointer(1, &first);
        let mut used = Vec::new();
        for version in 2..10 {
            let label = format!("plant.z{version}");
            definitions.extend(create_subjects(&[label.as_str()], 1));
            cluster.change(2, base, &definitions);
            base = pointer(version, &definitions);
            cluster.run(seconds(8));
            let board = cluster.board.lock().unwrap();
            let seen = board.specs.get(&3).and_then(|seen| seen.pointer);
            used.push(seen.map(|pointer| pointer.version));
        }
        let applied = cluster.board().applied;
        assert_eq!(
            applied.iter().filter(|a| a.2.is_ok()).count(),
            9,
            "seed {seed}"
        );
        if used.iter().all(|version| *version < Some(1)) {
            behind.push((seed, used));
        }
    }
    assert_eq!(behind, Vec::new());
}

// Voter 3 lacks the chunks of the first change. With 400 ms each way on the link of
// voters 1 and 3 and voter 2 silent, voter 3 gets some of them from voter 1, which then
// goes silent while voter 2 answers. Voter 2 then applies a change each 8 s. A get
// that a newer pointer drops at voter 1, which gave no chunk for that pointer, makes
// the next get start at voter 2.
#[test]
fn a_last_giver_that_goes_silent_keeps_no_voter_behind() {
    let mut late = Vec::new();
    for seed in 0..8 {
        let (mut cluster, first) = lacking(seed);
        cluster.board.lock().unwrap().silent = [2].into();
        let config = link::Config {
            delay: Span::from_nanos(400 * Span::MILLISECOND.nanos()),
            ..link::Config::default()
        };
        let (a, b) = (cluster.node(1).clone(), cluster.node(3).clone());
        cluster.sim.link(&a, &b, config);
        cluster.sim.link(&b, &a, config);
        cluster.start_voter(3);
        cluster.run(seconds(12));
        let blobs = cluster.board().blobs;
        assert!(
            blobs.get(&1).is_some_and(|served| *served > 0),
            "seed {seed}"
        );
        cluster.board.lock().unwrap().silent = [1].into();
        let mut definitions = first.clone();
        let mut base = pointer(1, &first);
        let mut used = Vec::new();
        for version in 2..10 {
            let label = format!("plant.z{version}");
            definitions.extend(create_subjects(&[label.as_str()], 1));
            cluster.change(2, base, &definitions);
            base = pointer(version, &definitions);
            cluster.run(seconds(8));
            let board = cluster.board.lock().unwrap();
            let seen = board.specs.get(&3).and_then(|seen| seen.pointer);
            used.push(seen.map(|pointer| pointer.version));
        }
        if used.last() != Some(&Some(9)) {
            late.push((seed, used));
        }
    }
    assert_eq!(late, Vec::new());
}

// As the 8 s test, with 400 ms each way on the link of voters 2 and 3, so that a newer
// pointer often drops a get while voter 2 gives. The next get starts at voter 2 again,
// not at silent voter 1.
#[test]
fn a_drop_while_the_last_giver_gives_keeps_the_start_at_it() {
    let mut late = Vec::new();
    for seed in 0..32 {
        let (mut cluster, first) = lacking(seed);
        cluster.board.lock().unwrap().silent = [1].into();
        let config = link::Config {
            delay: Span::from_nanos(400 * Span::MILLISECOND.nanos()),
            ..link::Config::default()
        };
        let (a, b) = (cluster.node(2).clone(), cluster.node(3).clone());
        cluster.sim.link(&a, &b, config);
        cluster.sim.link(&b, &a, config);
        cluster.start_voter(3);
        cluster.run(seconds(1));
        let mut definitions = first.clone();
        let mut base = pointer(1, &first);
        let mut used = Vec::new();
        for version in 2..22 {
            let label = format!("plant.z{version}");
            definitions.extend(create_subjects(&[label.as_str()], 1));
            cluster.change(2, base, &definitions);
            base = pointer(version, &definitions);
            cluster.run(seconds(8));
            let board = cluster.board.lock().unwrap();
            let seen = board.specs.get(&3).and_then(|seen| seen.pointer);
            used.push(seen.map(|pointer| pointer.version));
        }
        if used.last() != Some(&Some(21)) {
            late.push((seed, used));
        }
    }
    assert_eq!(late, Vec::new());
}

// Voter 3 is down at the change of 40 subjects, and at its start its store holds the
// tree of 20 of them. The read of one of those fails once, so the first read ends
// with `Cause::Blob`. The retry still gets each listed chunk that the store lacks in
// one round.
#[test]
fn a_failed_store_read_keeps_the_listed_round() {
    let (mut cluster, definitions) = lacking(0);
    let half: BTreeMap<Name, Definition> = definitions
        .iter()
        .take(20)
        .map(|(name, definition)| (name.clone(), definition.clone()))
        .collect();
    let mut chunks = Chunks::default();
    let full = spec::region::tree(&mut chunks, &definitions);
    let mut chunks = Chunks::default();
    let part = spec::region::tree(&mut chunks, &half);
    let shared: Vec<Digest> = part
        .chunks
        .iter()
        .copied()
        .filter(|digest| full.chunks.contains(digest))
        .collect();
    assert!(shared.len() > 3, "{}", shared.len());
    let first = *shared.iter().min().unwrap();
    cluster.board();
    cluster.board.lock().unwrap().puts = [(3, half)].into();
    let path = Path::new(BLOB).join(first.to_string());
    cluster.node(3).fail_file(&path, Operation::ReadAt);
    cluster.start_voter(3);
    let mut seen = Vec::new();
    for _ in 0..100 {
        cluster.run(TICK);
        let board = cluster.board.lock().unwrap();
        seen.extend(board.specs.get(&3).and_then(|seen| seen.behind.clone()));
    }
    let board = cluster.board();
    let blob = seen
        .iter()
        .any(|behind| matches!(behind.cause, Cause::Blob(_)));
    assert!(blob, "{seen:?}");
    assert_used(&board, &[3], pointer(1, &definitions), &definitions);
    let lacked = full.chunks.len() - shared.len();
    let calls = lacked.div_ceil(crate::driver::peer::CHUNKS);
    assert_eq!(board.blobs, [(1, calls)].into());
}

// Cuts the power of voter 3, and starts it again after `down` runs on its node.
fn restart<F: Future<Output = ()> + 'static>(
    cluster: &mut Cluster,
    down: impl FnOnce(sim::node::Node) -> F + Send + 'static,
) {
    let node = cluster.node(3).clone();
    cluster.sim.crash(&node, Crash::Power);
    let (own, board) = (node.clone(), Arc::clone(&cluster.board));
    let config = env::shards::Config {
        name: "voter-3".into(),
        core: None,
    };
    let main = move |tasks| async move {
        down(own.clone()).await;
        voter(own, tasks, 3, board).await;
    };
    drop(node.shards().start(config, main).unwrap());
}
