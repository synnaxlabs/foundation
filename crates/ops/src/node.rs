//! The handles that the operations on one node use.

use std::fmt;
use std::path::{Path, PathBuf};

use env::clock::Clock;
use serde_json::Value;
use types::channel;
use types::name::Name;

use crate::front_end::{File, FrontEnds};
use crate::plan::Documents;
use crate::roots::Failures;
use crate::{apply, used};

#[cfg(test)]
mod tests;

/// The handles that the operations on one node use. `node` builds one on shard 0,
/// where the mesh of the node runs. It is not `Send`.
pub struct Node {
    pub(crate) mesh: mesh::Mesh,
    pub(crate) keys: Box<dyn Fn() -> channel::Key>,
    pub(crate) front_ends: FrontEnds,
    pub(crate) supervisor: connector::supervisor::Supervisor,
    pub(crate) hub: hub::Hub,
    pub(crate) clock: Clock,
    pub(crate) name: Name,
    pub(crate) roots: Failures,
}

/// The dependencies of a [`Node`].
pub struct Config {
    /// The mesh that the operations read and change.
    pub mesh: mesh::Mesh,
    /// Gives the key of each new channel: each call gives a key that no channel holds
    /// and no earlier call gave.
    pub keys: Box<dyn Fn() -> channel::Key>,
    /// The syntax of each file extension.
    pub front_ends: FrontEnds,
    /// The connector supervisor of the node. `plan` and `apply` check with its kinds,
    /// and `plan` reads each secret of each connector of the node through its secrets,
    /// as a run does. A remote `plan` reads only those of each connector that its
    /// subject may apply.
    pub supervisor: connector::supervisor::Supervisor,
    /// The hub of the node, through which `status` reads the status channels.
    pub hub: hub::Hub,
    /// The clock of the node, on which `status` waits for the status channels.
    pub clock: Clock,
    /// The name of the node.
    pub name: Name,
    /// The failures of the read of the OS roots of the node. `status` gives them to a
    /// subject with `read` on the name of the node.
    pub roots: Failures,
}

impl Node {
    /// The node with the dependencies of `config`.
    #[must_use]
    pub fn new(config: Config) -> Self {
        let Config {
            mesh,
            keys,
            front_ends,
            supervisor,
            hub,
            clock,
            name,
            roots,
        } = config;
        Self {
            mesh,
            keys,
            front_ends,
            supervisor,
            hub,
            clock,
            name,
            roots,
        }
    }

    /// The mesh that the operations read and change.
    #[must_use]
    pub fn mesh(&self) -> &mesh::Mesh {
        &self.mesh
    }

    /// Plans the change from `files`, each a path and its text, to the spec in use,
    /// and reads each secret of each connector of this node. Gives the plan file and
    /// the JSON output that `plan --json` writes.
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
        let used = used::spec(&self.mesh).await.and_then(used::Spec::newest);
        let (base, applied) = used.map_err(|e| e.json())?;
        let planned = async {
            let documents = Documents::read(&files, &self.front_ends)?;
            documents.plan(self, base, &applied, |_| true).await
        };
        let (output, plan) = planned.await.map_err(|e| e.json())?;
        Ok((plan.encode(), crate::value(&output)))
    }

    /// Applies the plan file `bytes`, read from `path`, and gives the JSON output that
    /// `apply --json` writes.
    ///
    /// # Errors
    ///
    /// The JSON error that `apply --json` writes.
    pub async fn apply(&self, path: &Path, bytes: &[u8]) -> Result<Value, Value> {
        apply::file(path, bytes, &self.mesh, self.supervisor.kinds(), &self.keys)
            .await
            .map(|applied| crate::value(&applied))
            .map_err(|e| e.json())
    }
}

impl fmt::Debug for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("front_ends", &self.front_ends)
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("front_ends", &self.front_ends)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}
