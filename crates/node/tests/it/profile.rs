//! `foundation profile`, which prints the `node` block that pins a node.

use std::net::SocketAddr;

use document::Source;

use crate::rig::Rig;
use crate::{not_running, text};

/// The block reads back as a profile that pins the key that `--json` gives.
#[test]
fn the_block_pins_the_key_of_the_node_of_data() {
    let mut rig = Rig::new();
    rig.start_with(&["--name", "edge", "--data", "plant"]);
    let output = rig.run(&["profile", "--data", "plant", "10.0.0.5:7400"], b"");
    assert_eq!((output.status.code(), text(&output.stderr)), (Some(0), ""));
    let read = config_hcl::read(Source(0), text(&output.stdout))
        .and_then(|document| config::profile::decode(&document))
        .expect("a profile");
    let address: SocketAddr = "10.0.0.5:7400".parse().expect("an address");
    assert_eq!(read.nodes.len(), 1);
    let pinned = read.nodes[&address];
    let output = rig.run(
        &["profile", "--data", "plant", "--json", "10.0.0.5:7400"],
        b"",
    );
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("a JSON object");
    assert_eq!(
        json,
        serde_json::json!({
            "address": "10.0.0.5:7400",
            "public_key": config::openssh::line(pinned),
        })
    );
}

/// A data directory on which no node runs gives the failure of a reach.
#[test]
fn a_data_directory_with_no_node_is_refused() {
    let rig = Rig::new();
    let output = rig.run(&["profile", "--data", "plant", "10.0.0.5:7400"], b"");
    assert_eq!(
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        ),
        (Some(1), "", not_running("plant").as_str())
    );
}
