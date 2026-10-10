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
/// supervisor. Dropped, it drops each run. Each name has one loop, a future that runs
/// its connector, and after each change the connector of that change, until a change
/// removes it. So no two runs of one name overlap, also across a removal and an
/// addition.
pub(crate) struct Runs {
    supervisor: Supervisor,
    /// The name of the node in the region.
    node: Name,
    scope: Scope,
    /// The loop of each name, also one whose connector is gone, until the first apply
    /// after it ended.
    loops: BTreeMap<Name, Rc<RefCell<Loop>>>,
}

/// The state that `apply` and the loop of one name share.
struct Loop {
    /// `None` once the spec removed the connector.
    connector: Option<Connector>,
    /// Cancelled when `connector` changes. It cancels the run in progress.
    changed: cancel::Token,
    /// The loop read `None` and ended.
    ended: bool,
}

impl Runs {
    /// Runs no connector of the node `node`, its name in the region, yet. Each loop is
    /// a future on the tasks of `supervisor`, on a supervisor made from it.
    pub(crate) fn new(supervisor: supervisor::Config, node: Name) -> Self {
        Self {
            scope: Scope::new(supervisor.tasks.clone()),
            supervisor: Supervisor::new(supervisor),
            node,
            loops: BTreeMap::new(),
        }
    }

    /// Makes the runs match the connectors of `definitions` on the node. Cancels the
    /// run of each connector that is gone or changed, and starts a run of each one
    /// that is new or changed, after the last run of its name returned. Each other
    /// connector keeps its run.
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
        self.loops.retain(|name, state| {
            let mut state = state.borrow_mut();
            if state.ended {
                return false;
            }
            let wanted = wanted.get(name).copied();
            if state.connector.as_ref() != wanted {
                state.connector = wanted.cloned();
                state.changed.cancel();
                state.changed = cancel::Token::new();
            }
            true
        });
        for (name, connector) in wanted {
            if self.loops.contains_key(name) {
                continue;
            }
            let state = Rc::new(RefCell::new(Loop {
                connector: Some(connector.clone()),
                changed: cancel::Token::new(),
                ended: false,
            }));
            self.loops.insert(name.clone(), Rc::clone(&state));
            let supervisor = self.supervisor.clone();
            let name = name.clone();
            self.scope.spawn(Box::pin(async move {
                loop {
                    let (connector, changed) = {
                        let mut state = state.borrow_mut();
                        let Some(connector) = state.connector.clone() else {
                            state.ended = true;
                            return;
                        };
                        (connector, state.changed.clone())
                    };
                    let kind = connector.kind().as_str();
                    let config = connector.config().document();
                    match supervisor.run(kind, name.clone(), config, &changed).await {
                        // The status holds its class, and the next change of the
                        // connector runs it again.
                        Ok(()) | Err(connector::kind::Error::Config(_)) => {}
                        Err(error) => {
                            unreachable!(
                                "invariant: `run` gives only `Config`: {error}"
                            )
                        }
                    }
                    changed.wait().await;
                }
            }));
        }
    }
}

#[cfg(test)]
mod tests;
