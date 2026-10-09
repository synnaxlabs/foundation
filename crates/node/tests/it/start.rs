//! `foundation start`: a node on a data directory until SIGTERM.

use crate::rig::{Process, Rig};
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

/// The bytes of the file `budget` of `rig`.
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads what the binary wrote to the disk"
)]
fn kept(rig: &Rig) -> Vec<u8> {
    std::fs::read(rig.budget()).expect("read the budget file")
}

#[test]
fn a_first_start_keeps_its_budgets_and_a_restart_leaves_them() {
    let mut rig = Rig::new();
    std::fs::remove_file(rig.budget()).expect("remove the budget file");
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
    std::fs::write(rig.budget(), [0xff; 39]).expect("write the budget file");
    assert_eq!(
        ended(&rig.run(&["start"], b"")),
        (
            Some(1),
            "",
            "error[node.budget]: the file `budget` in the data directory \
             foundation-data does not hold budgets that a node wrote\n\
             fix: Remove it, and the next start computes the budgets again from the \
             free memory and disk\n"
        )
    );
}

#[test]
fn a_kept_pool_budget_that_gives_a_shard_too_little_fails() {
    let rig = Rig::new();
    rig.keep(1024, 256 << 20);
    let output = rig.run(&["start", "--name", "edge"], b"");
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

#[test]
fn a_kept_disk_budget_that_holds_no_ring_fails() {
    let rig = Rig::new();
    rig.keep(1 << 30, 1);
    let output = rig.run(&["start", "--name", "edge"], b"");
    let (status, out, errors) = ended(&output);
    assert_eq!((status, out), (Some(1), ""));
    // The count of shards and the least ring depend on the cores of the host.
    let (message, fix) = errors.split_once('\n').expect("two lines");
    assert!(
        message.starts_with(
            "error[node.disk]: the disk budget 1B, which foundation-data keeps from \
             its first start, holds no ring on each of "
        ),
        "{errors}"
    );
    assert_eq!(
        fix,
        "fix: Remove the file `budget` in foundation-data, and the next start \
         computes the budgets again from the free memory and disk\n"
    );
}

#[test]
fn a_data_directory_that_the_user_cannot_write_fails() {
    use std::os::unix::fs::PermissionsExt;

    let rig = Rig::new();
    let data = rig.dir.join("foundation-data/data");
    let mode = |mode| std::fs::Permissions::from_mode(mode);
    std::fs::create_dir_all(&data).expect("make the directory");
    std::fs::set_permissions(&data, mode(0o555)).expect("make it read-only");
    let output = rig.run(&["start", "--name", "edge"], b"");
    std::fs::set_permissions(&data, mode(0o755)).expect("make it writable");
    assert_eq!(
        ended(&output),
        (
            Some(1),
            "",
            "error[node.data]: cannot use the data directory foundation-data: open of \
             lock failed with OS error 13\n\
             fix: Give with `--data` a directory that this user can make and write\n"
        )
    );
}

#[test]
fn a_new_data_directory_in_a_directory_that_the_user_cannot_write_fails() {
    use std::os::unix::fs::PermissionsExt;

    let rig = Rig::new();
    let read = rig.dir.join("read");
    let mode = |mode| std::fs::Permissions::from_mode(mode);
    std::fs::create_dir(&read).expect("make the directory");
    std::fs::set_permissions(&read, mode(0o555)).expect("make it read-only");
    let output = rig.run(&["start", "--data", "read", "--name", "edge"], b"");
    std::fs::set_permissions(&read, mode(0o755)).expect("make it writable");
    assert_eq!(
        ended(&output),
        (
            Some(1),
            "",
            "error[node.data]: cannot open or make the data directory read: \
             Permission denied (os error 13)\n\
             fix: Give with `--data` a directory that this user can make and write\n"
        )
    );
}

#[test]
fn a_key_file_that_holds_no_key_fails() {
    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    std::fs::write(rig.dir.join("foundation-data/data/node.key"), [0xff; 7])
        .expect("write the key file");
    assert_eq!(
        ended(&rig.run(&["start"], b"")),
        (
            Some(1),
            "",
            "error[node.failed]: the file `node.key` in the data directory is not a \
             node key; restore it from a backup of this node\n\
             fix: Fix the cause that the message states, then start the node again\n"
        )
    );
}

/// Linux only: the test reads where each thread waits from `/proc`.
#[cfg(target_os = "linux")]
#[test]
fn a_node_whose_standard_output_nobody_reads_exits_0_at_sigterm() {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let rig = Rig::new();
    let (reader, mut writer) = std::io::pipe().expect("make a pipe");
    let stdout = writer.try_clone().expect("clone the pipe");
    // It fills the pipe long before the node starts, and ends when the test drops
    // `reader`.
    #[expect(
        clippy::disallowed_methods,
        reason = "a process test fills a pipe of another process"
    )]
    std::thread::spawn(move || while writer.write_all(&[0; 4096]).is_ok() {});
    let node = Command::new(env!("CARGO_BIN_EXE_foundation"))
        .args(["start", "--name", "edge"])
        .current_dir(&rig.dir)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::null())
        .spawn()
        .expect("start the node");
    let mut node = Process(node);
    let pid = node.0.id().to_string();
    rig.wait(
        "a thread of the node waits in its write of the line",
        || {
            let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).expect("list");
            let waits: Vec<String> = (tasks.map(|task| task.expect("read").path()))
                .filter_map(|task| std::fs::read_to_string(task.join("wchan")).ok())
                .collect();
            let blocked = waits.iter().any(|wait| wait.contains("pipe_write"));
            blocked.then_some(()).ok_or_else(|| waits.join("\n"))
        },
    );
    let sent = Command::new("kill").arg(&pid).status().expect("run kill");
    assert!(sent.success(), "send SIGTERM to {pid}");
    let status = rig.wait("the node exits at SIGTERM", || {
        node.0
            .try_wait()
            .expect("check the node")
            .ok_or_else(String::new)
    });
    drop(reader);
    assert_eq!(status.code(), Some(0));
}
