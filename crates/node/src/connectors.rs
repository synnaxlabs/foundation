//! Runs the connectors that the spec in use places on this node.

use std::collections::BTreeMap;
use std::rc::Rc;

use connector::cancel;
use connector::supervisor::{self, Supervisor};
use spec::connector::Connector;
use spec::definition::Definition;
use types::name::Name;

use crate::scope::Scope;

/// The runs of the connectors that the spec in use places on one node, on one
/// supervisor. Dropped, it drops each run. It holds one run for each connector of the
/// spec on the node, and one for each name whose run was cancelled and has not
/// ended.
pub(crate) struct Runs {
    supervisor: Rc<Supervisor>,
    /// The name of the node in the region.
    node: Name,
    scope: Scope,
    /// The last run of each name, also one whose connector is gone, until it ended.
    last: BTreeMap<Name, Run>,
}

/// One run of a connector.
struct Run {
    /// The connector it runs, or `None` once the spec removed or changed it.
    connector: Option<Connector>,
    cancel: cancel::Token,
    /// Cancelled once the run returned, or once it was cancelled before it started.
    ended: cancel::Token,
}

impl Runs {
    /// Runs no connector of the node `node`, its name in the region, yet. Each run is
    /// a future on the tasks of `supervisor`, on a supervisor made from it.
    pub(crate) fn new(supervisor: supervisor::Config, node: Name) -> Self {
        Self {
            scope: Scope::new(supervisor.tasks.clone()),
            supervisor: Rc::new(Supervisor::new(supervisor)),
            node,
            last: BTreeMap::new(),
        }
    }

    /// Makes the runs match the connectors of `definitions` on the node. Cancels the
    /// run of each connector that is gone or changed, and starts a run of each one
    /// that is new or changed. A run starts only after the last run of its name
    /// returned, so no two runs of one name overlap, also across a removal and an
    /// addition. Each other connector keeps its run.
    pub(crate) fn apply(&mut self, definitions: &BTreeMap<Name, Definition>) {
        let wanted: BTreeMap<&Name, &Connector> = definitions
            .iter()
            .filter_map(|(name, definition)| match definition {
                Definition::Connector(connector) if *connector.node() == self.node => {
                    Some((name, connector))
                }
                _ => None,
            })
            .collect();
        self.last.retain(|name, run| {
            if run.connector.as_ref() != wanted.get(name).copied() {
                run.connector = None;
                run.cancel.cancel();
            }
            run.connector.is_some() || !run.ended.cancelled()
        });
        for (name, connector) in wanted {
            let after = match self.last.get(name) {
                Some(run) if run.connector.is_some() => continue,
                Some(run) => Some(run.ended.clone()),
                None => None,
            };
            let run = Run {
                connector: Some(connector.clone()),
                cancel: cancel::Token::new(),
                ended: cancel::Token::new(),
            };
            let supervisor = Rc::clone(&self.supervisor);
            let (cancel, ended) = (run.cancel.clone(), run.ended.clone());
            self.last.insert(name.clone(), run);
            let (name, connector) = (name.clone(), connector.clone());
            self.scope.spawn(Box::pin(async move {
                if let Some(after) = after {
                    after.wait().await;
                }
                // A run cancelled before it starts may have no status channels left.
                if !cancel.cancelled() {
                    let kind = connector.kind().as_str();
                    let config = connector.config().document();
                    match supervisor.run(kind, name, config, &cancel).await {
                        // The status holds its class, and the next change of the
                        // connector runs it again.
                        Ok(()) | Err(connector::kind::Error::Config(_)) => {}
                        Err(error) => {
                            unreachable!(
                                "invariant: `run` gives only `Config`: {error}"
                            )
                        }
                    }
                }
                ended.cancel();
            }));
        }
    }
}

#[cfg(test)]
mod tests;
