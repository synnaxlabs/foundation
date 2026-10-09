//! The handles that the operations on one node use.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::Value;
use types::channel;

use crate::front_end::{File, FrontEnd};
use crate::{apply, plan, used};

#[cfg(test)]
mod tests;

/// The handles that the operations on one node use. `node` builds one on shard 0,
/// where the mesh of the node runs. It is not `Send`.
pub struct Node {
    mesh: mesh::Mesh,
    key: Box<dyn Fn() -> channel::Key>,
    front_ends: BTreeMap<&'static str, FrontEnd>,
    kinds: connector::kind::Table,
}

impl Node {
    /// `key` gives the key of each new channel: each call gives a key that no channel
    /// holds and no earlier call gave. `front_ends` maps each file extension to its
    /// syntax, and `kinds` holds each connector kind of the node.
    ///
    /// # Panics
    ///
    /// When `front_ends` is empty.
    #[must_use]
    pub fn new(
        mesh: mesh::Mesh,
        key: impl Fn() -> channel::Key + 'static,
        front_ends: BTreeMap<&'static str, FrontEnd>,
        kinds: connector::kind::Table,
    ) -> Self {
        assert!(!front_ends.is_empty(), "`ops::Node` needs a front end");
        Self {
            mesh,
            key: Box::new(key),
            front_ends,
            kinds,
        }
    }

    /// The mesh that the operations read and change.
    #[must_use]
    pub fn mesh(&self) -> &mesh::Mesh {
        &self.mesh
    }

    /// Plans the change from `files`, each a path and its text, to the spec in use.
    /// Gives the plan file and the JSON output that `plan --json` writes.
    ///
    /// # Errors
    ///
    /// The JSON error that `plan --json` writes, with its code, message, and fix.
    pub async fn plan(
        &self,
        files: Vec<(PathBuf, String)>,
    ) -> Result<(Vec<u8>, Value), Value> {
        let files: Vec<File> = files
            .into_iter()
            .map(|(path, text)| File { path, text })
            .collect();
        let (base, applied) = used::spec(&self.mesh).await.map_err(|e| e.json())?;
        let (output, plan) = plan::plan(
            &files,
            base,
            &applied,
            &self.mesh.names(),
            &self.front_ends,
            &self.kinds,
        )
        .map_err(|e| e.json())?;
        Ok((plan.encode(), output.json()))
    }

    /// Applies the plan file `bytes`, read from `path`, and gives the JSON output that
    /// `apply --json` writes.
    ///
    /// # Errors
    ///
    /// The JSON error that `apply --json` writes.
    pub async fn apply(&self, path: &Path, bytes: &[u8]) -> Result<Value, Value> {
        apply::apply(path, bytes, &self.mesh, &self.key)
            .await
            .map(|applied| applied.json())
            .map_err(|e| e.json())
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("front_ends", &self.front_ends.keys())
            .finish_non_exhaustive()
    }
}
