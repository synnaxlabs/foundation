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
fn a_data_directory_with_a_newline_prints_one_line() {
    let mut rig = Rig::new();
    rig.start_with(&["--name", "edge", "--data", "a\nb"]);
    assert_eq!(
        ended(&rig.stop()),
        (
            Some(0),
            "node edge runs in a\\nb. Stop it with Ctrl-C.\n",
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
            "error[node.data]: cannot write the data directory plain: Not a directory \
             (os error 20)\n\
             fix: Let this user make and write plain and each file in it, or give \
             another directory with `--data`\n"
        )
    );
}

/// The path shows as the line of `a_data_directory_with_a_newline_prints_one_line`
/// shows it.
#[test]
fn a_data_directory_with_a_newline_that_is_a_file_fails_on_one_line() {
    let rig = Rig::new();
    std::fs::write(rig.dir.join("a\nb"), "").expect("write the file");
    assert_eq!(
        ended(&rig.run(&["start", "--data", "a\nb", "--name", "edge"], b"")),
        (
            Some(1),
            "",
            "error[node.data]: cannot write the data directory a\\nb: Not a directory \
             (os error 20)\n\
             fix: Let this user make and write a\\nb and each file in it, or give \
             another directory with `--data`\n"
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
    rig.keep(1024, rig.disk);
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

/// A file `budget` with a checksum that matches and the largest pool budget. On one
/// core, the part of shard 0 is the whole budget. Linux only: the test pins the node
/// with `taskset`.
#[cfg(target_os = "linux")]
#[test]
fn a_kept_pool_budget_too_large_for_the_host_fails_with_no_panic() {
    let rig = Rig::new();
    rig.keep(u64::MAX, rig.disk);
    let output = rig.run_on_one_core(&["start", "--name", "edge"], b"");
    let (status, out, errors) = ended(&output);
    assert_eq!((status, out), (Some(1), ""), "{errors}");
    assert_eq!(
        errors,
        "error[node.memory]: the pool budget 18446744073709551615B, which \
         foundation-data keeps from its first start, gives one of 1 shards a pool \
         that needs more address space than this host has\n\
         fix: Remove the file `budget` in foundation-data, and the next start \
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
            "error[node.data]: cannot write the data directory foundation-data: open \
             of lock failed with OS error 13\n\
             fix: Let this user make and write foundation-data and each file in it, or \
             give another directory with `--data`\n"
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
            "error[node.data]: cannot write the data directory read: Permission \
             denied (os error 13)\n\
             fix: Let this user make and write read and each file in it, or give \
             another directory with `--data`\n"
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

#[test]
fn a_file_of_the_data_directory_that_the_user_cannot_write_fails() {
    use std::os::unix::fs::PermissionsExt;

    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    let mode = |mode| std::fs::Permissions::from_mode(mode);
    for (file, call) in [("node.key", "open"), ("shard-0/ring", "open")] {
        let path = rig.dir.join("foundation-data/data").join(file);
        std::fs::set_permissions(&path, mode(0o400)).expect("make it read-only");
        let output = rig.run(&["start"], b"");
        std::fs::set_permissions(&path, mode(0o600)).expect("make it writable");
        assert_eq!(
            ended(&output),
            (
                Some(1),
                "",
                format!(
                    "error[node.data]: cannot write the data directory foundation-data: \
                     {call} of {file} failed with OS error 13\n\
                     fix: Let this user make and write foundation-data and each file in \
                     it, or give another directory with `--data`\n"
                )
                .as_str()
            ),
            "{file}"
        );
    }
}

#[test]
fn a_key_file_that_is_a_directory_fails_as_node_failed() {
    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    let key = rig.dir.join("foundation-data/data/node.key");
    std::fs::remove_file(&key).expect("remove the key file");
    std::fs::create_dir(&key).expect("make a directory in its place");
    assert_eq!(
        ended(&rig.run(&["start"], b"")),
        (
            Some(1),
            "",
            "error[node.failed]: cannot use the data directory: open of node.key \
             failed with OS error 21\n\
             fix: Fix the cause that the message states, then start the node again\n"
        )
    );
}

#[test]
fn a_ring_that_is_a_directory_fails_as_node_failed() {
    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    let buffer = rig.dir.join("foundation-data/data/shard-0/ring");
    std::fs::remove_file(&buffer).expect("remove the ring");
    std::fs::create_dir(&buffer).expect("make a directory in its place");
    assert_eq!(
        ended(&rig.run(&["start"], b"")),
        (
            Some(1),
            "",
            "error[node.failed]: cannot open the buffer of shard-0: a file call failed: \
             open of shard-0/ring failed with OS error 21\n\
             fix: Fix the cause that the message states, then start the node again\n"
        )
    );
}

/// Linux only: the test reads the threads of the node from `/proc`.
#[cfg(target_os = "linux")]
#[test]
fn a_node_whose_standard_output_is_closed_runs_and_exits_0_at_sigterm() {
    let rig = Rig::new();
    let (reader, writer) = std::io::pipe().expect("make a pipe");
    drop(reader);
    let mut node = rig.spawn(&["start", "--name", "edge"], writer.into());
    let pid = node.pid();
    // `show` starts before `files-0`, and ends only after its write.
    rig.wait("the thread `show` ends after its write of the line", || {
        let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).expect("list");
        let names: Vec<String> = (tasks.map(|task| task.expect("read").path()))
            .map(|task| std::fs::read_to_string(task.join("comm")).unwrap_or_default())
            .collect();
        let runs = |name: &str| names.iter().any(|n| n.trim_end() == name);
        let ended = runs("files-0") && !runs("show");
        ended.then_some(()).ok_or_else(|| names.concat())
    });
    node.term();
    let status = rig.wait("the node exits at SIGTERM", || {
        node.exited().ok_or_else(String::new)
    });
    assert_eq!((status.code(), node.errors().as_str()), (Some(0), ""));
}

/// Linux only: the test reads where each thread waits from `/proc`.
#[cfg(target_os = "linux")]
#[test]
fn a_node_whose_standard_output_nobody_reads_exits_0_at_sigterm() {
    use std::io::Write;

    let rig = Rig::new();
    let (reader, mut writer) = std::io::pipe().expect("make a pipe");
    let stdout = writer.try_clone().expect("clone the pipe");
    let wchan = |task: &std::path::Path| {
        std::fs::read_to_string(task.join("wchan")).unwrap_or_default()
    };
    let (sent, filler) = std::sync::mpsc::channel();
    // It fills the pipe, and ends when the test drops `reader`.
    #[expect(
        clippy::disallowed_methods,
        reason = "a process test fills a pipe of another process"
    )]
    std::thread::spawn(move || {
        let task = std::fs::read_link("/proc/thread-self").expect("read the task");
        let task = std::path::Path::new("/proc").join(task);
        sent.send(task).expect("the test waits for the task");
        while writer.write_all(&[0; 4096]).is_ok() {}
    });
    let filler = rig.wait("the filler sends its task", || {
        filler.try_recv().map_err(|error| error.to_string())
    });
    rig.wait("the pipe is full", || {
        let wait = wchan(&filler);
        wait.contains("pipe_write").then_some(()).ok_or(wait)
    });
    let mut node = rig.spawn(&["start", "--name", "edge"], stdout.into());
    let pid = node.pid();
    rig.wait(
        "a thread of the node waits in its write of the line",
        || {
            let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).expect("list");
            let waits: Vec<String> = (tasks.map(|task| task.expect("read").path()))
                .map(|task| wchan(&task))
                .collect();
            let blocked = waits.iter().any(|wait| wait.contains("pipe_write"));
            blocked.then_some(()).ok_or_else(|| waits.join("\n"))
        },
    );
    node.term();
    let status = rig.wait("the node exits at SIGTERM", || {
        node.exited().ok_or_else(String::new)
    });
    drop(reader);
    assert_eq!(status.code(), Some(0));
}
