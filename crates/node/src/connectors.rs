//! Runs the connectors that the spec in use places on this node.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use connector::cancel;
use connector::supervisor::Supervisor;
use env::tasks::Tasks;
use spec::connector::Connector;
use spec::definition::Definition;
use types::channel;
use types::name::Name;

use crate::scope::Scope;

/// The runs of the connectors that the spec in use places on one node, on one
/// supervisor. Dropped, it drops each run. Each name has one loop, a future that runs
/// its connector, and after each change the connector of that change, until a change
/// removes it. After a run that gave `Config`, the next apply runs the connector again,
/// also one that came before `run` returned. So no two runs of one name overlap, also
/// across a removal and an addition. The first run of a loop starts only once no open
/// of a loop whose first run has not started is pending, counted from the apply that
/// added or changed its connector. A change after the first run of a loop does not
/// wait.
pub(crate) struct Runs {
    supervisor: Supervisor,
    opens: Rc<Opens>,
    /// The name of the node in the region.
    node: Name,
    scope: Scope,
    /// The loop of each name, also one whose connector is gone, until the first apply
    /// after it ended.
    loops: BTreeMap<Name, Rc<RefCell<Loop>>>,
}

/// The count of the first opens of loops that have not resolved.
struct Opens {
    pending: Cell<usize>,
    /// Cancelled, and replaced, each time `pending` goes to 0.
    settled: RefCell<cancel::Token>,
}

impl Opens {
    /// Counts one open until the guard drops.
    fn begin(self: &Rc<Self>) -> Pending {
        self.pending.set(self.pending.get() + 1);
        Pending(Rc::clone(self))
    }

    /// Resolves once no open is pending. An open counted after the last 0 holds it.
    async fn settled(&self) {
        while self.pending.get() > 0 {
            let settled = self.settled.borrow().clone();
            settled.wait().await;
        }
    }
}

/// One pending open of [`Opens`].
struct Pending(Rc<Opens>);

impl Drop for Pending {
    fn drop(&mut self) {
        let pending = self.0.pending.get() - 1;
        self.0.pending.set(pending);
        if pending == 0 {
            self.0.settled.replace(cancel::Token::new()).cancel();
        }
    }
}

/// One connector as the spec in use places it.
#[derive(Clone, PartialEq)]
struct Placed {
    connector: Connector,
    /// The keys of its status channels: its status index first, then each channel on
    /// it. A new key means that a spec the node did not see removed the channel, also
    /// with an equal `connector`.
    status: Vec<channel::Key>,
}

/// The state that `apply` and the loop of one name share.
struct Loop {
    /// `None` once the spec removed the connector.
    placed: Option<Placed>,
    /// Cancelled when `placed` changes. It cancels the run in progress.
    changed: cancel::Token,
    /// Cancelled at each apply. A run that gave `Config` runs again once the token of
    /// its start is cancelled, also by an apply before `run` returned.
    applied: cancel::Token,
    /// The loop read `None` and ended.
    ended: bool,
    first: First,
}

/// The state of the first run of a loop.
enum First {
    /// Its first open is pending, and holds this count from the apply that added or
    /// changed the connector, so before any loop polls.
    Opening(#[expect(dead_code, reason = "held for its drop")] Pending),
    /// Its first open resolved, or the connector is gone, and no run started.
    Waiting,
    /// A call of `Opened::run` that no change cancelled started.
    Started,
}

impl Loop {
    /// Sets the placement to `wanted`, and cancels the run of the old one. Before the
    /// first run, a change counts the next open in `opens`, and a removal frees it.
    fn change(&mut self, wanted: Option<Placed>, opens: &Rc<Opens>) {
        self.placed = wanted;
        self.changed.cancel();
        self.changed = cancel::Token::new();
        match (&self.placed, &self.first) {
            (None, First::Opening(_)) => self.first = First::Waiting,
            (Some(_), First::Waiting) => self.first = First::Opening(opens.begin()),
            _ => {}
        }
    }

    /// Frees the count of a first open that resolved with `changed` not cancelled.
    /// Returns whether the first run waits for the other first opens.
    fn resolved(&mut self, changed: &cancel::Token) -> bool {
        if changed.cancelled() || matches!(self.first, First::Started) {
            return false;
        }
        self.first = First::Waiting;
        true
    }

    /// Marks the first run started, unless `changed` is cancelled.
    fn start(&mut self, changed: &cancel::Token) {
        if !changed.cancelled() {
            self.first = First::Started;
        }
    }
}

impl Runs {
    /// Runs no connector of the node `node`, its name in the region, yet. Each loop is
    /// a future on `tasks`, the tasks of the shard of `supervisor`.
    pub(crate) fn new(supervisor: Supervisor, tasks: Tasks, node: Name) -> Self {
        Self {
            scope: Scope::new(tasks),
            supervisor,
            opens: Rc::new(Opens {
                pending: Cell::new(0),
                settled: RefCell::new(cancel::Token::new()),
            }),
            node,
            loops: BTreeMap::new(),
        }
    }

    /// Makes the runs match the connectors of `definitions` on the node. Cancels the
    /// run of each connector that is gone or changed, and starts a run of each one
    /// that is new or changed, or whose last run gave `Config`, after the last run of
    /// its name returned. Each other connector keeps its run. A connector with a new
    /// key of a status channel is changed. The first run of a loop waits until no open
    /// of a loop whose first run has not started is pending.
    pub(crate) fn apply(&mut self, definitions: &BTreeMap<Name, Definition>) {
        let wanted: BTreeMap<&Name, Placed> = definitions
            .iter()
            .filter_map(|(name, definition)| match definition {
                Definition::Connector(connector) if *connector.node() == self.node => {
                    let placed = Placed {
                        connector: connector.clone(),
                        status: connector::status::defined(definitions, name)
                            .into_iter()
                            .map(|(_, channel)| channel.key)
                            .collect(),
                    };
                    Some((name, placed))
                }
                _ => None,
            })
            .collect();
        self.loops.retain(|name, state| {
            let mut state = state.borrow_mut();
            if state.ended {
                return false;
            }
            state.applied.cancel();
            state.applied = cancel::Token::new();
            let wanted = wanted.get(name);
            if state.placed.as_ref() != wanted {
                state.change(wanted.cloned(), &self.opens);
            }
            true
        });
        for (name, placed) in wanted {
            if self.loops.contains_key(name) {
                continue;
            }
            let state = Rc::new(RefCell::new(Loop {
                placed: Some(placed),
                changed: cancel::Token::new(),
                applied: cancel::Token::new(),
                ended: false,
                first: First::Opening(self.opens.begin()),
            }));
            self.loops.insert(name.clone(), Rc::clone(&state));
            let (supervisor, opens) = (self.supervisor.clone(), Rc::clone(&self.opens));
            let name = name.clone();
            self.scope
                .spawn(Box::pin(run(supervisor, opens, name, state)));
        }
    }
}

/// The loop of the name `name`: runs its connector until a change removes it. Its
/// first run starts once `opens` settled, with its own count freed.
async fn run(
    supervisor: Supervisor,
    opens: Rc<Opens>,
    name: Name,
    state: Rc<RefCell<Loop>>,
) {
    loop {
        let (connector, changed, applied) = {
            let mut state = state.borrow_mut();
            let Some(Placed { connector, .. }) = state.placed.clone() else {
                state.ended = true;
                return;
            };
            (connector, state.changed.clone(), state.applied.clone())
        };
        let kind = connector.kind().as_str();
        let config = connector.config().document();
        let opened = supervisor.open(kind, name.clone(), config, &changed);
        let opened = opened.await;
        let waits = state.borrow_mut().resolved(&changed);
        if waits {
            changed.race(opens.settled()).await;
            state.borrow_mut().start(&changed);
        }
        match opened.run().await {
            // The next change of the connector runs it again.
            Ok(()) => changed.wait().await,
            // The status holds its class. The fix can be in another definition, so the
            // next spec change runs it again.
            Err(connector::kind::Error::Config(_)) => applied.wait().await,
            Err(error) => unreachable!("invariant: `run` gives only `Config`: {error}"),
        }
    }
}

#[cfg(test)]
#[cfg(not(loom))]
mod tests;
