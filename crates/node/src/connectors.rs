//! Runs the connectors that the spec in use places on this node.

use std::cell::RefCell;
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
/// spec on the node, and one for each name whose run was cancelled and had not ended
/// at the last apply. Each name has at most two futures: a run that has not returned,
/// and the last run, which waits for it. A change of a run that waits changes the
/// connector it starts with.
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
    /// The connector it runs, or `None` once the spec removed or changed it. The run
    /// reads it when it starts.
    connector: Rc<RefCell<Option<Connector>>>,
    cancel: cancel::Token,
    /// Cancelled once the run before it returned, or `None` for the first run.
    after: Option<cancel::Token>,
    /// Cancelled once the run returned, or once it found no connector at its start.
    returned: cancel::Token,
}

impl Run {
    /// The run has not read its connector.
    fn waiting(&self) -> bool {
        self.after.as_ref().is_some_and(|after| !after.cancelled())
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
            let wanted = wanted.get(name).copied();
            let mut connector = run.connector.borrow_mut();
            if connector.as_ref() != wanted {
                if run.waiting() {
                    *connector = wanted.cloned();
                } else {
                    *connector = None;
                    run.cancel.cancel();
                }
            }
            connector.is_some() || !run.returned.cancelled()
        });
        for (name, connector) in wanted {
            let after = match self.last.get(name) {
                Some(run) if run.connector.borrow().is_some() => continue,
                Some(run) => Some(run.returned.clone()),
                None => None,
            };
            let run = Run {
                connector: Rc::new(RefCell::new(Some(connector.clone()))),
                cancel: cancel::Token::new(),
                after: after.clone(),
                returned: cancel::Token::new(),
            };
            let supervisor = self.supervisor.clone();
            let (cancel, returned) = (run.cancel.clone(), run.returned.clone());
            let read = Rc::clone(&run.connector);
            self.last.insert(name.clone(), run);
            let name = name.clone();
            self.scope.spawn(Box::pin(async move {
                if let Some(after) = after {
                    after.wait().await;
                }
                // `None` once a change removed the connector before the run started.
                let connector = read.borrow().clone();
                if let Some(connector) = connector {
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
