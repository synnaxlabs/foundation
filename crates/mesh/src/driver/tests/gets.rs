//! Tests of the read of the spec in use that gets a missing chunk from a peer, on a
//! cluster of three nodes.

use super::apply::{assert_used, base, create_subjects, pointer};
use super::*;
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

// Voter 3 is down at the apply of a change of 40 subjects, so it lacks each of more
// than 40 chunks. A get that waited `RETRY` would take more than 40 s. Voter 2 takes
// three puts at the apply, and voter 3 gets each of the 41 chunks from voter 1, the
// first holder.
#[test]
fn a_voter_that_lacks_many_chunks_gets_each_at_once() {
    let mut cluster = Cluster::new(0);
    cluster.start_voter(1);
    cluster.start_voter(2);
    cluster.run(seconds(5));
    let labels: Vec<String> = (0..40).map(|at| format!("plant.s{at}")).collect();
    let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
    let definitions = create_subjects(&labels, 512);
    cluster.change(1, base(), &definitions);
    cluster.run(seconds(10));
    cluster.start_voter(3);
    cluster.run(seconds(10));
    let board = cluster.board();
    assert_used(&board, &[3], pointer(1, &definitions), &definitions);
    assert_eq!(board.blobs, [(1, 41), (2, 3)].into());
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
