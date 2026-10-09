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
