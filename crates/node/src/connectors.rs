//! Runs the connectors that the spec in use places on this node.

use std::collections::BTreeMap;

use connector::cancel;
use connector::supervisor::{self, Supervisor};
use spec::connector::Connector;
use spec::definition::Definition;
use types::name::Name;

use crate::scope::Scope;

/// The runs of the connectors that the spec in use places on one node, on one
/// supervisor. Dropped, it drops each run. It holds one run for each connector of the
/// spec on the node, and one for each name whose run was cancelled and had not ended
/// at the last apply. Each name has at most two futures: a run that has not returned,
/// and the last run, which waits for it.
pub(crate) struct Runs {
    supervisor: Supervisor,
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
    /// Cancelled once each earlier run of the name ended, or `None` for the first.
    after: Option<cancel::Token>,
    /// Cancelled once the run returned, or once it was cancelled before it started.
    returned: cancel::Token,
}

impl Run {
    /// Cancelled once this run and each earlier run of its name ended, when this run
    /// is cancelled. A run cancelled before `after` never starts.
    fn ended(&self) -> &cancel::Token {
        match &self.after {
            Some(after) if !after.cancelled() => after,
            _ => &self.returned,
        }
    }
}

impl Runs {
    /// Runs no connector of the node `node`, its name in the region, yet. Each run is
    /// a future on the tasks of `supervisor`, on a supervisor made from it.
    pub(crate) fn new(supervisor: supervisor::Config, node: Name) -> Self {
        Self {
            scope: Scope::new(supervisor.tasks.clone()),
            supervisor: Supervisor::new(supervisor),
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
            run.connector.is_some() || !run.ended().cancelled()
        });
        for (name, connector) in wanted {
            let after = match self.last.get(name) {
                Some(run) if run.connector.is_some() => continue,
                Some(run) => Some(run.ended().clone()),
                None => None,
            };
            let run = Run {
                connector: Some(connector.clone()),
                cancel: cancel::Token::new(),
                after: after.clone(),
                returned: cancel::Token::new(),
            };
            let supervisor = self.supervisor.clone();
            let (cancel, returned) = (run.cancel.clone(), run.returned.clone());
            self.last.insert(name.clone(), run);
            let (name, connector) = (name.clone(), connector.clone());
            self.scope.spawn(Box::pin(async move {
                if let Some(after) = after {
                    cancel.race(after.wait()).await;
                }
                // The run before it may not have returned yet.
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
                returned.cancel();
            }));
        }
    }
}

#[cfg(test)]
mod tests;
