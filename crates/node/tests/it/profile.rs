//! `foundation profile`, which prints the `node` block that pins a node.

use std::net::SocketAddr;

use document::Source;
use types::ed25519::PublicKey;

use crate::rig::Rig;
use crate::text;

/// The public key of the node of the data directory `dir` of `rig`.
fn proved(rig: &Rig, dir: &str) -> PublicKey {
    let dir = rig.dir.join(dir);
    rig.wait("the node writes its address", || {
        sdk::Node::local(&dir).map_err(|e| e.to_string())
    })
    .0
    .public_key
}

/// The node `block` that `profile` prints, as a profile reads it back.
fn pinned(block: &str) -> Vec<(SocketAddr, PublicKey)> {
    let read = config_hcl::read(Source(0), block)
        .and_then(|document| config::profile::decode(&document))
        .unwrap_or_else(|problems| panic!("{problems:?}\n{block}"));
    read.nodes.into_iter().collect()
}

/// The `--json` output of `profile` on `plant` at `address`.
fn json(rig: &Rig, address: &str) -> serde_json::Value {
    let output = rig.run(&["profile", "--data", "plant", "--json", address], b"");
    assert_eq!((output.status.code(), text(&output.stderr)), (Some(0), ""));
    let stdout = text(&output.stdout);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    serde_json::from_str(stdout).expect("a JSON object")
}

/// The block and `--json` pin the key that the node of `--data` proves.
#[test]
fn the_block_pins_the_key_that_the_node_of_data_proves() {
    let mut rig = Rig::new();
    rig.start_with(&["--name", "edge", "--data", "plant"]);
    let public_key = proved(&rig, "plant");
    let output = rig.run(&["profile", "--data", "plant", "10.0.0.5:7400"], b"");
    assert_eq!((output.status.code(), text(&output.stderr)), (Some(0), ""));
    let address: SocketAddr = "10.0.0.5:7400".parse().expect("an address");
    assert_eq!(pinned(text(&output.stdout)), vec![(address, public_key)]);
    assert_eq!(
        json(&rig, "10.0.0.5:7400"),
        serde_json::json!({
            "address": "10.0.0.5:7400",
            "public_key": config::openssh::line(public_key),
        })
    );
}

/// Only Linux: [`Rig::spawn`] runs only there. The exit status and the standard error
/// of `args` with `stdout` as its output.
#[cfg(target_os = "linux")]
fn ended(
    rig: &Rig,
    args: &[&str],
    stdout: std::process::Stdio,
) -> (Option<i32>, String) {
    let mut process = rig.spawn(args, stdout);
    let status = rig.wait("the command exits", || {
        process.exited().ok_or_else(String::new)
    });
    (status.code(), process.errors())
}

/// Only Linux: `/dev/full` and [`Rig::spawn`] exist only there. A full disk under
/// standard output: `profile` reports it as `version` does.
#[cfg(target_os = "linux")]
#[test]
fn a_full_standard_output_is_reported_as_cli_reports_it() {
    let mut rig = Rig::new();
    rig.start_with(&["--name", "edge", "--data", "plant"]);
    #[expect(clippy::disallowed_methods, reason = "a standard output with no room")]
    let full = || {
        std::fs::File::create("/dev/full")
            .expect("open /dev/full")
            .into()
    };
    let version = ended(&rig, &["version"], full());
    let profile = ended(
        &rig,
        &["profile", "--data", "plant", "10.0.0.5:7400"],
        full(),
    );
    assert_eq!(
        version,
        (
            Some(1),
            "error[ops.output]: standard output could not be written: No space left \
             on device (os error 28)\nfix: Give standard output a destination that \
             can be written\n"
                .to_owned()
        )
    );
    assert_eq!(profile, version);
}

/// Only Linux: [`Rig::spawn`] runs only there. A closed standard output: `profile`
/// ends as `version` does.
#[cfg(target_os = "linux")]
#[test]
fn a_closed_standard_output_ends_as_cli_ends() {
    let mut rig = Rig::new();
    rig.start_with(&["--name", "edge", "--data", "plant"]);
    let closed = || {
        let (reader, writer) = std::io::pipe().expect("make a pipe");
        drop(reader);
        writer.into()
    };
    let profile = ended(
        &rig,
        &["profile", "--data", "plant", "10.0.0.5:7400"],
        closed(),
    );
    assert_eq!(profile, (Some(0), String::new()));
    assert_eq!(profile, ended(&rig, &["version"], closed()));
}

/// IPv6 addresses, with a scope and IPv4-mapped too, read back.
#[test]
fn ipv6_addresses_read_back() {
    let mut rig = Rig::new();
    rig.start_with(&["--name", "edge", "--data", "plant"]);
    let public_key = proved(&rig, "plant");
    for given in ["[::1]:7400", "[fe80::1%2]:7400", "[::ffff:10.0.0.5]:7400"] {
        let output = rig.run(&["profile", "--data", "plant", given], b"");
        assert_eq!((output.status.code(), text(&output.stderr)), (Some(0), ""));
        let address: SocketAddr = given.parse().expect("an address");
        assert_eq!(pinned(text(&output.stdout)), vec![(address, public_key)]);
    }
}

/// Only Unix: the test sets a file mode. A node whose `admin.key` this user cannot
/// read still gives its public key: the command reads no file other than `node.key`.
#[cfg(unix)]
#[test]
fn an_unreadable_admin_key_does_not_hide_the_public_key() {
    use std::os::unix::fs::PermissionsExt;
    let mut rig = Rig::new();
    rig.start_with(&["--name", "edge", "--data", "plant"]);
    let public_key = proved(&rig, "plant");
    let admin = rig.dir.join("plant/data/admin.key");
    let mode = |mode| {
        std::fs::set_permissions(&admin, std::fs::Permissions::from_mode(mode))
            .expect("chmod");
    };
    mode(0);
    let line = json(&rig, "10.0.0.5:7400")["public_key"].clone();
    mode(0o600);
    assert_eq!(line, config::openssh::line(public_key));
}

/// Only Unix: the test sets a file mode. A file `node.key` that this user cannot read
/// gives `node.unreadable`.
#[cfg(unix)]
#[test]
fn an_unreadable_node_key_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let mut rig = Rig::new();
    rig.start_with(&["--name", "edge", "--data", "plant"]);
    proved(&rig, "plant");
    rig.stop();
    let key = rig.dir.join("plant/data/node.key");
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o000))
        .expect("chmod");
    let output = rig.run(&["profile", "--data", "plant", "10.0.0.5:7400"], b"");
    assert_eq!(
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        ),
        (
            Some(1),
            "",
            "error[node.unreadable]: cannot read the data directory plant: open of \
             node.key failed with OS error 13\nfix: Run the command as the user that \
             started the node\n"
        )
    );
}

/// The `node` block that `profile` prints for `plant` of `rig` at `10.0.0.5:7400`.
fn block(rig: &Rig) -> String {
    let output = rig.run(&["profile", "--data", "plant", "10.0.0.5:7400"], b"");
    assert_eq!((output.status.code(), text(&output.stderr)), (Some(0), ""));
    text(&output.stdout).to_owned()
}

/// A node that stopped gives the block that it gave as it ran.
#[test]
fn a_node_that_stopped_gives_its_key() {
    let mut rig = Rig::new();
    rig.start_with(&["--name", "edge", "--data", "plant"]);
    proved(&rig, "plant");
    let running = block(&rig);
    assert_eq!(rig.stop().status.code(), Some(0));
    assert_eq!(block(&rig), running);
}

/// Only Unix: [`Rig::kill`] sends `SIGKILL`. A node that crashed gives its key.
#[cfg(unix)]
#[test]
fn a_node_that_crashed_gives_its_key() {
    let mut rig = Rig::new();
    rig.start_with(&["--name", "edge", "--data", "plant"]);
    let public_key = proved(&rig, "plant");
    rig.kill();
    assert_eq!(
        json(&rig, "10.0.0.5:7400")["public_key"],
        config::openssh::line(public_key)
    );
}

/// `profile` on `dir` of `rig` fails with `code`, `message`, and `fix`, as text and
/// as JSON.
fn refused(rig: &Rig, dir: &str, code: &str, message: &str, fix: &str) {
    let output = rig.run(&["profile", "--data", dir, "10.0.0.5:7400"], b"");
    assert_eq!(
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        ),
        (
            Some(1),
            "",
            format!("error[{code}]: {message}\nfix: {fix}\n").as_str()
        )
    );
    let output = rig.run(&["profile", "--data", dir, "--json", "10.0.0.5:7400"], b"");
    let error = serde_json::json!({ "errors": [{ "code": code, "message": message,
        "fix": fix, "notes": [] }] });
    assert_eq!(
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        ),
        (Some(1), "", format!("{error}\n").as_str())
    );
}

/// A data directory that is not there, or that holds no `node.key`, has no node that
/// started.
#[test]
fn a_data_directory_with_no_node_is_refused() {
    let rig = Rig::new();
    let unstarted = |rig: &Rig| {
        refused(
            rig,
            "plant",
            "node.unstarted",
            "no node has started in plant",
            "Start the node once with `foundation start --data=plant`, with `--name \
             <name>`",
        );
    };
    unstarted(&rig);
    std::fs::create_dir_all(rig.dir.join("plant/data")).expect("make the directory");
    unstarted(&rig);
}

/// A `--data` that is a file, or whose `data` is a file, is no data directory.
#[test]
fn a_file_is_no_data_directory() {
    let rig = Rig::new();
    std::fs::write(rig.dir.join("plant"), b"").expect("write a file");
    std::fs::create_dir_all(rig.dir.join("yard")).expect("make the directory");
    std::fs::write(rig.dir.join("yard/data"), b"").expect("write a file");
    for dir in ["plant", "yard"] {
        refused(
            &rig,
            dir,
            "node.unstarted",
            &format!("no node has started in {dir}, which is not a data directory"),
            "Give `--data` the data directory of a node that has started once",
        );
    }
}

/// A node that started and then lost `node.key` lost its key: a new start makes no
/// key of this node.
#[test]
fn a_lost_node_key_is_refused() {
    let mut rig = Rig::new();
    rig.start_with(&["--name", "edge", "--data", "plant"]);
    proved(&rig, "plant");
    assert_eq!(rig.stop().status.code(), Some(0));
    std::fs::remove_file(rig.dir.join("plant/data/node.key")).expect("remove the file");
    refused(
        &rig,
        "plant",
        "node.failed",
        "the data directory has the directory of a mesh, and the file `node.key` is \
         not there or holds a key that the founding of its region does not hold; \
         restore `node.key` from a backup of this node, or give another data \
         directory",
        "Fix the cause that the message states, then run the command again",
    );
}

/// A file `node.key` that a node did not write is refused.
#[test]
fn a_foreign_node_key_is_refused() {
    let rig = Rig::new();
    std::fs::create_dir_all(rig.dir.join("plant/data")).expect("make the directory");
    std::fs::write(rig.dir.join("plant/data/node.key"), b"not a key").expect("write");
    refused(
        &rig,
        "plant",
        "node.failed",
        "the file `node.key` in the data directory is not a node key; restore it from \
         a backup of this node",
        "Fix the cause that the message states, then run the command again",
    );
}

/// Only Unix: the test sets a file mode. A data directory that this user cannot open
/// gives `node.unreadable`, not `node.unstarted`.
#[cfg(unix)]
#[test]
fn an_unreadable_data_directory_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new();
    let data = rig.dir.join("plant/data");
    std::fs::create_dir_all(&data).expect("make the directory");
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o000))
        .expect("chmod");
    let output = rig.run(&["profile", "--data", "plant", "10.0.0.5:7400"], b"");
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o700))
        .expect("chmod");
    assert_eq!(
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        ),
        (
            Some(1),
            "",
            "error[node.unreadable]: cannot read the data directory plant: \
             Permission denied (os error 13)\nfix: Run the command as the user that \
             started the node\n"
        )
    );
}
