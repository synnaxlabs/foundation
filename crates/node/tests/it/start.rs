//! `foundation start`: a node on a data directory until SIGTERM.

use crate::rig::Rig;
use crate::text;

/// The exit status, the standard output, and the standard error of `output`.
fn ended(output: &std::process::Output) -> (Option<i32>, &str, &str) {
    (
        output.status.code(),
        text(&output.stdout),
        text(&output.stderr),
    )
}

const RUNS: &str = "node edge runs in foundation-data. Stop it with Ctrl-C.\n";

#[test]
fn a_node_prints_that_it_runs_and_exits_0_at_sigterm() {
    let mut rig = Rig::new();
    rig.start();
    assert_eq!(ended(&rig.stop()), (Some(0), RUNS, ""));
}

#[test]
fn a_node_prints_that_it_runs_as_json() {
    let mut rig = Rig::new();
    rig.start_with(&["--name", "edge", "--json"]);
    assert_eq!(
        ended(&rig.stop()),
        (
            Some(0),
            "{\"name\":\"edge\",\"data\":\"foundation-data\"}\n",
            ""
        )
    );
}

#[test]
fn a_restart_reads_the_name_from_the_data_directory() {
    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    rig.start_with(&[]);
    assert_eq!(ended(&rig.stop()), (Some(0), RUNS, ""));
}

#[test]
fn a_second_node_on_a_data_directory_fails_and_the_first_runs_on() {
    let mut rig = Rig::new();
    rig.start();
    assert_eq!(
        ended(&rig.run(&["start", "--name", "edge"], b"")),
        (
            Some(1),
            "",
            "error[node.busy]: another node runs in foundation-data\n\
             fix: Stop that node, or give another data directory with `--data`\n"
        )
    );
    assert_eq!(ended(&rig.stop()), (Some(0), RUNS, ""));
}

#[test]
fn a_restart_with_another_name_fails() {
    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    assert_eq!(
        ended(&rig.run(&["start", "--name", "cloud"], b"")),
        (
            Some(1),
            "",
            "error[node.renamed]: the data directory foundation-data holds the node \
             edge, not cloud\n\
             fix: Give `--name edge`, or another data directory with `--data`\n"
        )
    );
}

#[test]
fn a_first_start_with_no_name_fails() {
    let rig = Rig::new();
    assert_eq!(
        ended(&rig.run(&["start"], b"")),
        (
            Some(1),
            "",
            "error[node.unnamed]: the data directory foundation-data holds no node\n\
             fix: Give the new node a name with `--name`\n"
        )
    );
}

#[test]
fn a_name_file_that_holds_no_name_fails() {
    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    std::fs::write(rig.dir.join("foundation-data/data/name"), [0xff; 277])
        .expect("write the name file");
    assert_eq!(
        ended(&rig.run(&["start"], b"")),
        (
            Some(1),
            "",
            "error[node.name]: the file `name` in the data directory foundation-data \
             is not a node name\n\
             fix: Remove it, and start the node with its name\n"
        )
    );
}

#[test]
fn a_data_directory_that_is_a_file_fails() {
    let rig = Rig::new();
    std::fs::write(rig.dir.join("plain"), "").expect("write the file");
    assert_eq!(
        ended(&rig.run(&["start", "--data", "plain", "--name", "edge"], b"")),
        (
            Some(1),
            "",
            "error[node.data]: cannot open or make the data directory plain: \
             Not a directory (os error 20)\n\
             fix: Give with `--data` a directory that this user can make and write\n"
        )
    );
}

/// The path of the file `budget` of the rig's data directory.
fn budget(rig: &Rig) -> std::path::PathBuf {
    rig.dir.join("foundation-data/data/budget")
}

/// The bytes of the file `budget` of `rig`.
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads what the binary wrote to the disk"
)]
fn kept(rig: &Rig) -> Vec<u8> {
    std::fs::read(budget(rig)).expect("read the budget file")
}

#[test]
fn a_first_start_keeps_its_budgets_and_a_restart_leaves_them() {
    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    let first = kept(&rig);
    assert_eq!(first.len(), 39);
    assert_eq!(&first[..19], b"foundation/budget/1");
    assert_eq!(first[35..], crc32c::crc32c(&first[..35]).to_le_bytes());
    let number =
        |at: usize| u64::from_le_bytes(first[at..][..8].try_into().expect("8 bytes"));
    let (pool, disk) = (number(19), number(27));
    assert!((1..=1 << 30).contains(&pool), "pool {pool}");
    assert!((1..=8 << 30).contains(&disk), "disk {disk}");
    rig.start_with(&[]);
    assert_eq!(ended(&rig.stop()), (Some(0), RUNS, ""));
    assert_eq!(kept(&rig), first);
}

#[test]
fn a_budget_file_that_holds_no_budgets_fails() {
    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    std::fs::write(budget(&rig), [0xff; 39]).expect("write the budget file");
    assert_eq!(
        ended(&rig.run(&["start"], b"")),
        (
            Some(1),
            "",
            "error[node.budget]: the file `budget` in the data directory \
             foundation-data is not a node's budgets\n\
             fix: Remove it, and the next start computes the budgets again from the \
             free memory and disk\n"
        )
    );
}

#[test]
fn a_kept_pool_budget_that_gives_a_shard_too_little_fails() {
    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    let mut bytes = kept(&rig);
    bytes[19..27].copy_from_slice(&1024_u64.to_le_bytes());
    let sum = crc32c::crc32c(&bytes[..35]);
    bytes[35..].copy_from_slice(&sum.to_le_bytes());
    std::fs::write(budget(&rig), bytes).expect("write the budget file");
    let output = rig.run(&["start"], b"");
    let (status, out, errors) = ended(&output);
    assert_eq!((status, out), (Some(1), ""));
    // The shard and the block sizes depend on the cores of the host.
    let (message, fix) = errors.split_once('\n').expect("two lines");
    assert!(
        message.starts_with(
            "error[node.memory]: the pool budget 1KiB, which foundation-data keeps \
             from its first start, gives shard-"
        ),
        "{errors}"
    );
    assert_eq!(
        fix,
        "fix: Remove the file `budget` in foundation-data, and the next start \
         computes the budgets again from the free memory and disk\n"
    );
}
