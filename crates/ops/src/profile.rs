//! The output of `foundation profile`.

use std::net::SocketAddr;

use document::Document;
use document::encoding::Checked;
use serde::{Serialize, Serializer};
use types::ed25519::PublicKey;

/// Where a program on another host dials a node, and the public key that the node
/// proves.
#[derive(Serialize)]
pub(crate) struct Pin {
    pub(crate) address: SocketAddr,
    /// The line of an OpenSSH `.pub` file in JSON.
    #[serde(serialize_with = "line")]
    pub(crate) public_key: PublicKey,
}

impl Pin {
    /// The `node` block of the pin, in HCL.
    pub(crate) fn text(&self) -> String {
        let node = config::profile::node(self.address, self.public_key);
        let document = Document {
            blocks: vec![node],
            ..Document::default()
        };
        let document =
            Checked::new(document).expect("invariant: a node block is shallow");
        config_hcl::write(&document).expect("invariant: HCL holds a node block")
    }
}

fn line<S: Serializer>(
    public_key: &PublicKey,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&config::openssh::line(*public_key))
}
