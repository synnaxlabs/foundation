//! A simulated OPC UA server for the tests of other crates: the open62541 test server,
//! with no security, that serves variables of the `DataType` `BaseDataType`, each with
//! a Double value, and records each read that it answers.
//!
//! It also serves one fixed tree to browse and import. Each node id other than that of
//! `Plant.Odd.guid` is `ns=1;s=` and the path with `.`, such as
//! `ns=1;s=Plant.Line.Speed`:
//!
//! - `Plant`, a folder that the Objects folder `Organizes`.
//! - `Plant.Types`, a folder that `Plant` organizes, with one variable of each type
//!   `Boolean`, `SByte`, `Byte`, `Int16`, `UInt16`, `Int32`, `UInt32`, `Int64`,
//!   `UInt64`, `Float`, `Double`, `String`, `DateTime`, `Guid`, and `ByteString`,
//!   named as its type, each with a fixed value.
//! - `Plant.Line`, a folder of `Plant` by `HasComponent`, which organizes `Plant`, a
//!   loop. Its components:
//!   - `Temperature`, the Double 21.5, with the property `EngineeringUnits` `°C`
//!     (UN/CEFACT `CEL`);
//!   - `Speed`, the value 1.25 of the data type `ns=1;s=Speed`, a subtype of Double;
//!   - `Mode`, the value 0 of `ns=1;s=Mode`, a subtype of `Enumeration`;
//!   - `Profile`, an array of the Doubles 1.5 and 2.5;
//!   - `Locked`, a Double that a client cannot read;
//!   - `Recipe`, of `ns=1;s=Recipe`, a subtype of `Structure`.
//! - `Plant.Odd`, a folder that `Plant` organizes, with:
//!   - `guid`, a folder of the node id `ns=1;g=00000001-0002-0003-0400-000000000000`;
//!   - `Level`, the Double 2.5, by `HasComponent` and again by `Organizes`;
//!   - `Twin`, the Double 3.5 with the browse name `Level`.
//!
//! The tree also adds the data types `ns=1;s=Speed`, `ns=1;s=Mode`, and
//! `ns=1;s=Recipe`, and the types Double (`ns=0;i=11`), `Structure` (`ns=0;i=22`), and
//! `Enumeration` (`ns=0;i=29`), which the namespace 0 of the build lacks. The
//! variables of the tree hold their values with no change, and the server does not
//! record their reads. A page of a Browse holds at most 8 references, so `Plant.Types`
//! takes two pages.

use std::collections::BTreeMap;
use std::ffi::CString;
use std::sync::{Arc, Mutex, MutexGuard};

use connector::cancel::Token;
use env::clock::Clock;
use env::net::{self, Listener, Net};
use env::rng::Rng;
use types::quality::Quality;
use types::time::{Monotonic, Span, Stamp};

use crate::connection::Manager;
use crate::event;
pub use crate::log::call_refused;
use crate::node::Node;
use crate::server::Server;
use crate::value::Value;

/// The sessions and secure channels of [`serve`].
const SESSIONS: u16 = 4;

/// The variables of [`serve`], each with its Double value and its status, each read
/// that the server answered, and the clock of the server. Clones share them.
#[derive(Clone, Debug)]
pub struct Variables(Arc<Mutex<State>>);

#[derive(Debug)]
struct State {
    variables: Vec<Variable>,
    /// The status of each later read.
    quality: Quality,
    /// The span of the server clock past the wall time of [`serve`].
    ahead: Span,
    /// The most nodes of a Read that a server answers, set before it starts.
    #[cfg(test)]
    most: Option<u32>,
    /// The most session timeout of a server, set before it starts.
    #[cfg(test)]
    session: Option<Span>,
    /// The reads of the state of the server (`ns=0;i=2259`) that a server answered.
    #[cfg(test)]
    states: u32,
    /// The status of each answer of the state of the server, at first Good.
    #[cfg(test)]
    answer: Quality,
}

#[derive(Debug)]
struct Variable {
    /// The text of the node in [`Variables::new`].
    text: String,
    node: Node,
    value: Value,
    served: Vec<Served>,
    #[cfg(test)]
    hooks: Hooks,
}

/// What a test sets on a variable.
#[cfg(test)]
#[derive(Debug, Default)]
struct Hooks {
    /// The status of each later read, in place of the status of all.
    quality: Option<Quality>,
    /// The value that a server holds in place of `value`, set before it starts.
    fixed: Option<Fixed>,
}

/// A value of a variable that is not a scalar of [`Value`], which a server holds
/// with no change and the status Good, and does not record.
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum Fixed {
    /// The String `text`.
    String,
    /// An array of two Doubles.
    Doubles,
}

impl Variables {
    /// The variables at the node ids `nodes`, each with the value 0 and the status
    /// Good.
    ///
    /// # Panics
    ///
    /// If a text of `nodes` is not a node id, or two name the same node.
    #[must_use]
    pub fn new(nodes: &[&str]) -> Self {
        let mut variables: Vec<Variable> = Vec::with_capacity(nodes.len());
        for &text in nodes {
            let node: Node = (text.parse())
                .unwrap_or_else(|error| panic!("{text:?} is not a node id: {error}"));
            if let Some(twin) = variables.iter().find(|variable| variable.node == node)
            {
                panic!("{text:?} and {:?} name the same node", twin.text);
            }
            variables.push(Variable {
                text: text.into(),
                node,
                value: Value::F64(0.0),
                served: Vec::new(),
                #[cfg(test)]
                hooks: Hooks::default(),
            });
        }
        Self(Arc::new(Mutex::new(State {
            variables,
            quality: Quality::GOOD,
            ahead: Span::ZERO,
            #[cfg(test)]
            most: None,
            #[cfg(test)]
            session: None,
            #[cfg(test)]
            states: 0,
            #[cfg(test)]
            answer: Quality::GOOD,
        })))
    }

    /// Adds 1 to the value of each variable, and gives each later read the status
    /// `quality`.
    pub fn change(&self, quality: Quality) {
        let mut state = self.state();
        for variable in &mut state.variables {
            if let Value::F64(value) = &mut variable.value {
                *value += 1.0;
            }
        }
        state.quality = quality;
    }

    /// Puts the clock of the server `ahead` of the `wall` time of [`serve`], in the
    /// value of `CurrentTime`, in each server and source timestamp, and in the time of
    /// each channel token. A later call moves it, as a jump of the clock does. A
    /// negative `ahead` puts it behind. Past the last stamp or before the first, the
    /// clock stays there.
    pub fn set_clock(&self, ahead: Span) {
        self.state().ahead = ahead;
    }

    /// Sets the value of the variable of `text` in [`Variables::new`]. A value that is
    /// not a Double keeps its value at [`Variables::change`], and its reads are served
    /// but not recorded, since a [`Served`] holds an `f64`.
    ///
    /// # Panics
    ///
    /// If no variable has `text`.
    #[cfg(test)]
    pub(crate) fn set(&self, text: &str, value: Value) {
        let mut state = self.state();
        let variable = state.variables.iter_mut().find(|v| v.text == text);
        variable
            .unwrap_or_else(|| panic!("no variable {text:?}"))
            .value = value;
    }

    /// Gives each later read of the variable of `text` the status `quality`, in place
    /// of the status of [`Variables::change`].
    ///
    /// # Panics
    ///
    /// If no variable has `text`.
    #[cfg(test)]
    pub(crate) fn set_quality(&self, text: &str, quality: Quality) {
        self.variable(text, |variable| variable.hooks.quality = Some(quality));
    }

    /// Makes a server that starts later hold `fixed` at the variable of `text`.
    ///
    /// # Panics
    ///
    /// If no variable has `text`.
    #[cfg(test)]
    pub(crate) fn fix(&self, text: &str, fixed: Fixed) {
        self.variable(text, |variable| variable.hooks.fixed = Some(fixed));
    }

    /// Makes a server refuse each Read of more than `most` nodes, and not tell the
    /// client the limit.
    #[cfg(test)]
    pub(crate) fn refuse_reads_past(&self, most: u32) {
        self.state().most = Some(most);
    }

    /// Makes a server give each session a timeout of at most `most`.
    #[cfg(test)]
    pub(crate) fn time_out_sessions(&self, most: Span) {
        self.state().session = Some(most);
    }

    /// Gives each later answer of the state of the server the status `quality`.
    #[cfg(test)]
    pub(crate) fn answer_states(&self, quality: Quality) {
        self.state().answer = quality;
    }

    /// The reads of the state of the server (`ns=0;i=2259`) that a server answered.
    #[cfg(test)]
    pub(crate) fn states(&self) -> u32 {
        self.state().states
    }

    /// The most nodes of a Read of [`Variables::refuse_reads_past`], the timeout of
    /// [`Variables::time_out_sessions`], and the value of [`Variables::fix`] of each
    /// variable.
    #[cfg(test)]
    fn hooks(&self) -> (Option<u32>, Option<Span>, Vec<Option<Fixed>>) {
        let state = self.state();
        let fixed = state.variables.iter().map(|v| v.hooks.fixed).collect();
        (state.most, state.session, fixed)
    }

    /// Calls `change` on the variable of `text`.
    #[cfg(test)]
    fn variable(&self, text: &str, change: impl FnOnce(&mut Variable)) {
        let mut state = self.state();
        let variable = state.variables.iter_mut().find(|v| v.text == text);
        change(variable.unwrap_or_else(|| panic!("no variable {text:?}")));
    }

    /// Each read that a server answered, by the text of its node in
    /// [`Variables::new`], in the order of the answers.
    #[must_use]
    pub fn served(&self) -> BTreeMap<String, Vec<Served>> {
        (self.state().variables.iter())
            .map(|variable| (variable.text.clone(), variable.served.clone()))
            .collect()
    }

    /// Answers a read of variable `index` at `time`, and records it when its value is
    /// a Double.
    fn read(&self, index: usize, time: Monotonic) -> (Value, Quality) {
        let mut state = self.state();
        let quality = state.quality;
        let variable = &mut state.variables[index];
        #[cfg(test)]
        let quality = variable.hooks.quality.unwrap_or(quality);
        if let Value::F64(value) = variable.value {
            let served = Served {
                time,
                value,
                quality,
            };
            variable.served.push(served);
        }
        (variable.value, quality)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().expect("no holder of the variables panics")
    }
}

/// A read that a server answered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Served {
    /// The time of the answer, on the clock of the server.
    pub time: Monotonic,
    /// The value of the answer.
    pub value: f64,
    /// The status of the answer.
    pub quality: Quality,
}

/// Serves `variables` on `listener` with room for 4 sessions until `until` is
/// cancelled or the listener fails, then closes the server. It adds each namespace
/// from 2 to the highest of `variables`. Each stream of the server has the options of
/// `listener`. The server clock is the wall time of `wall` plus the span of
/// [`Variables::set_clock`]. A client logs a warning when the time of a channel token
/// is 10 s or more from its own: give the wall time of the client's node. Call it on a
/// thread that `env` started. It also serves the tree of the module doc.
///
/// The close stops the server and drives each stream while the delayed callbacks of
/// open62541 drain: each stream sends what it can in that time, then a FIN. An answer
/// in flight can be cut. The close waits on no peer: a stream that still has bytes to
/// send at its end drops, which resets it. On each return, each stream that it
/// accepted has ended, and the port of `listener` is free. A drop of the future while
/// it waits closes the server with no wait, drops each stream with what waits on it
/// unsent, which resets it, and frees the port. A drop while the thread panics leaks
/// the server.
///
/// # Errors
///
/// The error of the first accept that fails, also when `until` is cancelled in the
/// same step.
///
/// # Panics
///
/// If open62541 refuses the server, a namespace, a variable, or a step of the close.
/// If a variable has the number 0, which open62541 would serve at a free number. If a
/// variable has the node id of a node that the tree adds.
///
/// # Aborts
///
/// When open62541 cannot allocate the loop or the connection manager: the process
/// stops as the Rust allocator does when it has no memory. When `wall` panics:
/// open62541 calls it, and a panic cannot unwind through C.
pub async fn serve(
    clock: Clock,
    wall: impl Fn() -> Stamp + 'static,
    net: Net,
    listener: Listener,
    rng: &mut Rng,
    variables: &Variables,
    until: Token,
) -> Result<(), net::Error> {
    let shifted = Variables::clone(variables);
    let time = event::Time {
        clock: Clock::clone(&clock),
        wall: Box::new(move || {
            let nanos = wall().nanos().saturating_add(shifted.state().ahead.nanos());
            Stamp::from_nanos(nanos)
        }),
    };
    let manager = Manager::accepting(time, net, listener, rng);
    let nodes: Vec<Node> = (variables.state().variables.iter())
        .map(|variable| variable.node.clone())
        .collect();
    #[cfg(test)]
    let (most, session, fixed) = variables.hooks();
    let add = |server: &mut Server| {
        server.cap_references(tree::REFERENCES);
        tree::add(server);
        #[cfg(test)]
        if let Some(most) = most {
            server.refuse_reads_past(most);
        }
        #[cfg(test)]
        if let Some(most) = session {
            server.set_session_timeout(most);
        }
        #[cfg(test)]
        {
            let variables = Variables::clone(variables);
            server.set_source(&state_node(), move || {
                let mut state = variables.state();
                state.states += 1;
                (Value::I32(0), state.answer)
            });
        }
        let top = nodes.iter().map(|node| node.namespace).max().unwrap_or(0);
        for namespace in 2..=top {
            let uri = CString::new(format!("urn:foundation:sim:{namespace}"))
                .expect("invariant: a URI with no NUL");
            let added = server.add_namespace(&uri);
            assert_eq!(added, namespace, "open62541 refused a namespace");
        }
        for (index, node) in nodes.iter().enumerate() {
            #[cfg(test)]
            if let Some(fixed) = fixed[index] {
                match fixed {
                    Fixed::String => server.add_string(node),
                    Fixed::Doubles => server.add_doubles(node),
                }
                continue;
            }
            let (variables, clock) =
                (Variables::clone(variables), Clock::clone(&clock));
            server.add_source(node, move || variables.read(index, clock.now()));
        }
    };
    Server::serve(manager, SESSIONS, add, until.wait()).await
}

/// Runs the [`serve`] that `serve` gives for a new token until `work` ends, then
/// cancels the token, waits for the serve, and gives the output of `work`.
///
/// # Panics
///
/// When the serve ends before `work`, or fails.
#[cfg(test)]
pub(crate) async fn during<F: Future<Output = Result<(), net::Error>>, T>(
    serve: impl FnOnce(Token) -> F,
    work: impl Future<Output = T>,
) -> T {
    let until = Token::new();
    let serve = serve(until.clone());
    let (mut serve, mut work) = (std::pin::pin!(serve), std::pin::pin!(work));
    let out = std::future::poll_fn(|cx| {
        if let std::task::Poll::Ready(end) = serve.as_mut().poll(cx) {
            panic!("the serve ended before its work: {end:?}");
        }
        work.as_mut().poll(cx)
    })
    .await;
    until.cancel();
    serve.await.expect("the listener does not fail");
    out
}

/// The state of a server, whose reads a test server answers with `Running`, with the
/// status of [`Variables::answer_states`], and counts.
#[cfg(test)]
fn state_node() -> Node {
    Node {
        namespace: 0,
        key: crate::node::Key::Number(crate::ffi::SERVER_STATE),
    }
}

#[cfg(test)]
mod tests;
pub(crate) mod tree;
