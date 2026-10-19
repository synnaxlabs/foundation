//! `foundation start`: a node on a data directory until SIGTERM.

use crate::quickstart::blocks;
use crate::rig::{self, Rig};
use crate::{after_stamp, logged, text};

/// The exit status, the standard output, and the standard error of `output` after the
/// log of the node.
///
/// # Panics
///
/// When the log is not the events of a run that `start` gives: none, `started` then
/// `stopped`, or `stopped` alone.
fn ended(output: &std::process::Output) -> (Option<i32>, &str, &str) {
    let (log, errors) = logged(&output.stderr);
    let events: Vec<String> = log.lines().map(event).collect();
    let events: Vec<&str> = events.iter().map(String::as_str).collect();
    assert!(
        [
            &[][..],
            &["INFO node: started", "INFO node: stopped"],
            &["INFO node: started", "ERROR node: stopped"],
            &["ERROR node: stopped"],
        ]
        .contains(&&events[..]),
        "not the log of a run: {log:?}"
    );
    (output.status.code(), text(&output.stdout), errors)
}

/// The level, the target, and the message of the text or JSON log line `line`.
fn event(line: &str) -> String {
    if line.starts_with('{') {
        let line: serde_json::Value = serde_json::from_str(line).expect("a JSON line");
        let part =
            |value: &serde_json::Value| value.as_str().expect("a string").to_owned();
        let (level, target) = (part(&line["level"]), part(&line["target"]));
        format!("{level} {target}: {}", part(&line["fields"]["message"]))
    } else {
        let line = after_stamp(line).expect("a stamp");
        line.split(' ').take(3).collect::<Vec<_>>().join(" ")
    }
}

/// `log` with the stamp and the field `mesh_time` cut from each line: the OS gives
/// both.
fn unstamped(log: &str) -> Vec<&str> {
    (log.lines())
        .map(|line| {
            let line = after_stamp(line).expect("a stamp");
            line.split_once(" mesh_time=")
                .map_or(line, |(line, _)| line)
        })
        .collect()
}

#[test]
fn a_node_logs_that_it_started_and_that_it_stopped_before_its_lines() {
    let mut rig = Rig::new();
    rig.start();
    let output = rig.stop();
    let (log, errors) = logged(&output.stderr);
    let [started, stopped] = unstamped(log)[..] else {
        panic!("not two lines: {log:?}")
    };
    let shards = os::shards().expect("read the cores").cores();
    let address = (started.strip_prefix("INFO node: started name=edge address="))
        .and_then(|rest| rest.strip_suffix(&format!(" shards={shards}")))
        .unwrap_or_else(|| panic!("not the line of a start: {started:?}"));
    address.parse::<std::net::SocketAddr>().expect("an address");
    assert_eq!((stopped, errors), ("INFO node: stopped", ""));
}

#[test]
fn a_node_that_fails_logs_its_error_before_its_lines() {
    let rig = Rig::new();
    let output = rig.run(&["start", "--name", "@x"], b"");
    let (log, errors) = logged(&output.stderr);
    let error = "the name @x has a segment that starts with `@`, which is reserved for \
                 Foundation; give another name";
    assert_eq!(
        unstamped(log),
        [format!("ERROR node: stopped error=\"{error}\"").as_str()]
    );
    assert!(errors.starts_with("error[node.reserved]: "), "{errors:?}");
}

#[test]
fn a_node_with_json_logs_json() {
    let mut rig = Rig::new();
    rig.start_with(&["--name", "edge", "--json"]);
    let output = rig.stop();
    let (log, _) = logged(&output.stderr);
    let messages: Vec<_> = (log.lines())
        .map(|line| {
            let line: serde_json::Value = serde_json::from_str(line).expect("JSON");
            line["fields"]["message"].clone()
        })
        .collect();
    assert_eq!(messages, ["started", "stopped"]);
}

/// What `foundation start --name edge` prints to standard output on a first start.
fn runs() -> &'static str {
    started().1
}

/// The start block of the quickstart page: the log of a first start on stderr, then
/// its standard output.
fn started() -> (&'static str, &'static str) {
    let shown: Vec<_> = blocks("text")
        .filter(|block| block.contains(" INFO node: started "))
        .collect();
    let [block] = shown[..] else {
        panic!("the page has {} start blocks, not one", shown.len());
    };
    logged(block.as_bytes())
}

/// Each word of the first line of `log` after its stamp, with no value after a `=`.
fn shape(log: &str) -> Vec<&str> {
    (unstamped(log)[0].split(' '))
        .map(|word| word.split_once('=').map_or(word, |(key, _)| key))
        .collect()
}

#[test]
fn a_node_prints_that_it_runs_and_exits_0_at_sigterm() {
    let mut rig = Rig::new();
    rig.start();
    let output = rig.stop();
    let (log, stdout) = started();
    assert_eq!(ended(&output), (Some(0), stdout, ""));
    assert_eq!(shape(logged(&output.stderr).0), shape(log));
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
    let data = rig.dir.join("a\nb");
    #[expect(
        clippy::disallowed_methods,
        reason = "the test reads what the rig wrote to the disk"
    )]
    let kept = std::fs::read(data.join("data/budget")).expect("read the budget file");
    assert_eq!(kept, rig::budget(rig::POOL, rig.disk));
    // The disk budget bounds the length of each ring, not the other files of the node.
    let entries =
        std::fs::read_dir(data.join("data")).expect("read the data directory");
    let rings = (entries.map(|entry| entry.expect("read an entry").file_name()))
        .filter(|name| name.to_str().is_some_and(|name| name.starts_with("shard-")))
        .map(|shard| {
            let path = data.join("data").join(shard).join("ring");
            std::fs::metadata(path).expect("read the ring").len()
        })
        .sum::<u64>();
    assert!((1..=rig.disk).contains(&rings), "{rings} bytes of rings");
}

#[test]
fn a_restart_reads_the_name_from_the_data_directory() {
    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    rig.start_with(&[]);
    assert_eq!(ended(&rig.stop()), (Some(0), runs(), ""));
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
    assert_eq!(ended(&rig.stop()), (Some(0), runs(), ""));
}

#[test]
fn a_first_start_founds_the_region_and_a_restart_needs_no_admin_key() {
    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    let admin = rig.dir.join("foundation-data/data/admin.key");
    let len = std::fs::metadata(&admin).expect("stat the admin key").len();
    assert_eq!(len, 54);
    std::fs::remove_file(&admin).expect("remove the admin key");
    rig.start_with(&[]);
    assert_eq!(ended(&rig.stop()), (Some(0), runs(), ""));
    assert!(!admin.exists(), "a restart made a new admin key");
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
fn a_first_start_with_a_reserved_name_fails_and_a_start_with_another_name_runs() {
    let mut rig = Rig::new();
    assert_eq!(
        ended(&rig.run(&["start", "--name", "@x"], b"")),
        (
            Some(1),
            "",
            "error[node.reserved]: the name @x has a segment that starts with `@`, \
             which is reserved for Foundation\n\
             fix: Give a name with no segment that starts with `@`\n"
        )
    );
    rig.start();
    assert_eq!(ended(&rig.stop()), (Some(0), runs(), ""));
}

#[test]
fn a_first_start_with_a_long_name_fails_and_a_start_with_191_bytes_runs() {
    let mut rig = Rig::new();
    let long = "a".repeat(192);
    let failed = format!(
        "error[node.long]: the name {long} is longer than 191 bytes, the most that \
         leaves room for the name of each status channel of the node\n\
         fix: Give a shorter name with `--name`\n"
    );
    let output = rig.run(&["start", "--name", &long], b"");
    assert_eq!(ended(&output), (Some(1), "", failed.as_str()));
    let name = "a".repeat(191);
    rig.start_with(&["--name", &name]);
    let runs = format!("node {name} runs in foundation-data. Stop it with Ctrl-C.\n");
    assert_eq!(ended(&rig.stop()), (Some(0), runs.as_str(), ""));
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
             fix: Make plain a directory that this user can write, and let it write \
             each file in it, or give another directory with `--data`\n"
        )
    );
}

/// The OS gives the program name, so it can read as a flag.
#[test]
fn a_failed_start_json_is_json_for_a_program_named_double_dash() {
    use std::os::unix::process::CommandExt;

    let rig = Rig::new();
    std::fs::write(rig.dir.join("plain"), "").expect("write the file");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_foundation"))
        .arg0("--")
        .args(["start", "--json", "--data", "plain", "--name", "edge"])
        .current_dir(&rig.dir)
        .output()
        .expect("foundation runs");
    assert_eq!(
        ended(&output),
        (
            Some(1),
            "",
            "{\"errors\":[{\"code\":\"node.data\",\"fix\":\"Make plain a directory \
             that this user can write, and let it write each file in it, or give \
             another directory with `--data`\",\"message\":\"cannot write the data \
             directory plain: Not a directory (os error 20)\",\"notes\":[]}]}\n"
        )
    );
}

#[test]
fn a_data_directory_whose_parent_is_missing_fails() {
    let rig = Rig::new();
    assert_eq!(
        ended(&rig.run(&["start", "--data", "missing/x", "--name", "edge"], b"")),
        (
            Some(1),
            "",
            "error[node.data]: cannot write the data directory missing/x: No such file \
             or directory (os error 2)\n\
             fix: Make missing/x a directory that this user can write, and let it \
             write each file in it, or give another directory with `--data`\n"
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
             fix: Make a\\nb a directory that this user can write, and let it write \
             each file in it, or give another directory with `--data`\n"
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
    assert_eq!(ended(&rig.stop()), (Some(0), runs(), ""));
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
    let cores = os::shards().expect("read the cores").cores().get();
    let least = types::byte::Size::from_bytes(62_272 * u64::try_from(cores).unwrap());
    assert_eq!(
        ended(&output),
        (
            Some(1),
            "",
            format!(
                "error[node.memory]: the pool budget 1KiB, which foundation-data keeps \
                 from its first start, opens no buffer on each of {cores} shards; it \
                 needs at least {least}\n\
                 fix: Remove the file `budget` in foundation-data, and the next start \
                 computes the budgets again from the free memory and disk\n"
            )
            .as_str()
        )
    );
}

/// A file `budget` with a checksum that matches and the largest pool budget. On one
/// core, the part of shard 0 is the whole budget.
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

/// A kept pool budget of 1 PiB, whose part on one core is under `usize::MAX` bytes and
/// more than the address space of the host.
#[test]
fn a_kept_pool_budget_that_the_system_cannot_reserve_fails() {
    let rig = Rig::new();
    rig.keep(1 << 50, rig.disk);
    let output = rig.run_on_one_core(&["start", "--name", "edge"], b"");
    let (status, out, errors) = ended(&output);
    assert_eq!((status, out), (Some(1), ""), "{errors}");
    assert_eq!(
        errors,
        "error[node.memory]: the pool budget 1024TiB, which foundation-data keeps \
         from its first start, needs more address space for shard-0 than the system \
         gives: a reserve of 108086391056891968 bytes of address space failed: Cannot \
         allocate memory (os error 12); lower the memory budget\n\
         fix: Remove the file `budget` in foundation-data, and the next start \
         computes the budgets again from the free memory and disk\n"
    );
}

/// A first start under an address space of 1 GiB, which its pool budget does not fit.
#[cfg_attr(
    target_os = "macos",
    ignore = "only Linux: macOS does not enforce a limit of address space"
)]
#[cfg_attr(
    windows,
    ignore = "only Linux: Windows has no limit on the address space of a process"
)]
#[test]
fn a_first_pool_budget_that_the_system_cannot_reserve_fails() {
    let rig = Rig::new();
    std::fs::remove_file(rig.budget()).expect("remove the budget file");
    let output = rig.run_on_one_core_in(1 << 30, &["start", "--name", "edge"], b"");
    let (status, out, errors) = ended(&output);
    assert_eq!((status, out), (Some(1), ""), "{errors}");
    // The size of the reserve depends on the free memory of the host.
    let (message, fix) = errors.split_once('\n').expect("two lines");
    let size = (message.strip_prefix(
        "error[node.failed]: no memory for the pool of shard-0: a reserve of ",
    ))
    .and_then(|rest| {
        rest.strip_suffix(
            " bytes of address space failed: Cannot allocate memory (os error 12); \
             lower the memory budget",
        )
    });
    assert!(
        size.is_some_and(|size| size.parse::<u64>().is_ok()),
        "{errors}"
    );
    assert_eq!(
        fix,
        "fix: Fix the cause that the message states, then run the command again\n"
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
             fix: Make foundation-data a directory that this user can write, and let \
             it write each file in it, or give another directory with `--data`\n"
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
             fix: Make read a directory that this user can write, and let it write \
             each file in it, or give another directory with `--data`\n"
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
             fix: Fix the cause that the message states, then run the command again\n"
        )
    );
}

#[test]
fn a_region_with_no_name_file_and_no_key_file_fails_with_and_with_no_name() {
    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    for file in ["name", "node.key"] {
        let path = rig.dir.join("foundation-data/data").join(file);
        std::fs::remove_file(path).expect("remove the file");
    }
    for args in [&["start", "--name", "other"][..], &["start"]] {
        assert_eq!(
            ended(&rig.run(args, b"")),
            (
                Some(1),
                "",
                "error[node.failed]: the data directory has the directory of a mesh, \
                 and the file `node.key` is not there or holds a key that the founding \
                 of its region does not hold; restore `node.key` from a backup of this \
                 node, or give another data directory\n\
                 fix: Fix the cause that the message states, then run the command \
                 again\n"
            ),
            "{args:?}"
        );
    }
}

#[test]
fn a_file_of_the_data_directory_that_the_user_cannot_write_fails() {
    use std::os::unix::fs::PermissionsExt;

    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    let mode = |mode| std::fs::Permissions::from_mode(mode);
    for (file, call) in [("shard-0/ring", "open"), ("mesh/log/log-0", "open")] {
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
                     fix: Make foundation-data a directory that this user can write, \
                     and let it write each file in it, or give another directory \
                     with `--data`\n"
                )
                .as_str()
            ),
            "{file}"
        );
    }
}

/// A start only reads `node.key`, so a key that the user cannot write starts.
#[test]
fn a_key_that_the_user_cannot_write_starts() {
    use std::os::unix::fs::PermissionsExt;

    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    let path = rig.dir.join("foundation-data/data/node.key");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400))
        .expect("make it read-only");
    rig.start();
    assert_eq!(ended(&rig.stop()), (Some(0), runs(), ""));
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
             fix: Fix the cause that the message states, then run the command again\n"
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
             fix: Fix the cause that the message states, then run the command again\n"
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
    let errors = node.errors();
    assert_eq!((status.code(), logged(errors.as_bytes()).1), (Some(0), ""));
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

#[test]
fn an_admin_key_that_holds_no_key_fails() {
    let rig = Rig::new();
    let data = rig.dir.join("foundation-data/data");
    std::fs::create_dir_all(&data).expect("make the directory");
    std::fs::write(data.join("admin.key"), [0xff; 54]).expect("write the admin key");
    assert_eq!(
        ended(&rig.run(&["start", "--name", "edge"], b"")),
        (
            Some(1),
            "",
            "error[node.failed]: the file `admin.key` in the data directory is not an \
             admin key; restore it from a backup, or remove it to make a new admin\n\
             fix: Fix the cause that the message states, then run the command again\n"
        )
    );
}

#[test]
fn a_start_on_a_port_that_another_socket_holds_fails() {
    #[expect(
        clippy::disallowed_methods,
        reason = "a process test holds a port that the node then asks for"
    )]
    let held = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind a socket");
    let at = held.local_addr().expect("the bound address").to_string();
    let rig = Rig::new();
    assert_eq!(
        ended(&rig.run(&["start", "--name", "edge", "--listen", &at], b"")),
        (
            Some(1),
            "",
            &*format!(
                "error[node.port]: cannot bind the node's port at {at}: address {at} \
                 is in use\n\
                 fix: Give `--listen` an address of this host with a port that is free \
                 and that this user can bind\n"
            )
        )
    );
}

#[test]
fn a_start_at_a_multicast_ip_fails() {
    let rig = Rig::new();
    let listen = ["start", "--name", "edge", "--listen", "224.0.0.1:0"];
    assert_eq!(
        ended(&rig.run(&listen, b"")),
        (
            Some(1),
            "",
            "error[node.port]: cannot bind the node's port at 224.0.0.1:0: a \
             multicast or broadcast IP is not an address of a host\n\
             fix: Give `--listen` an address of this host with a port that is free \
             and that this user can bind\n"
        )
    );
}

#[test]
fn a_start_at_a_multicast_ip_on_a_running_node_fails_for_the_ip() {
    let mut rig = Rig::new();
    rig.start();
    let listen = ["start", "--name", "edge", "--listen", "224.0.0.1:0"];
    assert_eq!(
        ended(&rig.run(&listen, b"")),
        (
            Some(1),
            "",
            "error[node.port]: cannot bind the node's port at 224.0.0.1:0: a \
             multicast or broadcast IP is not an address of a host\n\
             fix: Give `--listen` an address of this host with a port that is free \
             and that this user can bind\n"
        )
    );
    assert_eq!(ended(&rig.stop()), (Some(0), runs(), ""));
}

#[test]
fn a_chunk_store_that_the_user_cannot_list_fails() {
    use std::os::unix::fs::PermissionsExt;

    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    let blob = rig.dir.join("foundation-data/data/blob");
    let mode = |mode| std::fs::Permissions::from_mode(mode);
    std::fs::set_permissions(&blob, mode(0o000)).expect("make it closed");
    let output = rig.run(&["start"], b"");
    std::fs::set_permissions(&blob, mode(0o755)).expect("make it open");
    assert_eq!(
        ended(&output),
        (
            Some(1),
            "",
            "error[node.data]: cannot write the data directory foundation-data: list \
             of blob failed with OS error 13\n\
             fix: Make foundation-data a directory that this user can write, and let \
             it write each file in it, or give another directory with `--data`\n"
        )
    );
}

#[test]
fn a_listen_that_is_no_socket_address_fails() {
    let rig = Rig::new();
    assert_eq!(
        ended(&rig.run(&["start", "--name", "edge", "--listen", "x"], b"")),
        (
            Some(2),
            "",
            "error[ops.argument]: invalid value for one of the arguments: \
             `--listen <LISTEN>`: invalid socket address syntax\n\
             fix: Match the arguments to `foundation --help` or \
             `foundation <operation> --help`\n"
        )
    );
}

#[test]
fn a_start_with_another_name_after_the_name_file_is_gone_fails() {
    let mut rig = Rig::new();
    rig.start();
    rig.stop();
    std::fs::remove_file(rig.dir.join("foundation-data/data/name"))
        .expect("remove the name file");
    assert_eq!(
        ended(&rig.run(&["start", "--name", "other"], b"")),
        (
            Some(1),
            "",
            "error[node.renamed]: the data directory foundation-data holds the node \
             edge, not other\n\
             fix: Give `--name edge`, or another data directory with `--data`\n"
        )
    );
    rig.start_with(&[]);
    assert_eq!(ended(&rig.stop()), (Some(0), runs(), ""));
    rig.start();
    assert_eq!(ended(&rig.stop()), (Some(0), runs(), ""));
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test reads what the binary wrote to the disk"
)]
fn a_node_holds_the_file_address_while_it_runs_and_writes_it_empty_at_sigterm() {
    let mut rig = Rig::new();
    let address = rig.dir.join("foundation-data/data/address");
    rig.start();
    let held = std::fs::read(&address).expect("read the file address");
    assert_eq!(held.len(), 99);
    assert_eq!(held[..20], *b"foundation/address/1");
    assert_eq!(held[95..], crc32c::crc32c(&held[..95]).to_le_bytes());
    assert_eq!(ended(&rig.stop()), (Some(0), runs(), ""));
    let stopped = std::fs::read(&address).expect("read the file address");
    assert_eq!(stopped, [0; 99]);
}
