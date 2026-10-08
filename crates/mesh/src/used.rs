//! The spec that a node uses.

use std::collections::BTreeMap;
use std::rc::Rc;

use env::files;
use spec::definition::Definition;
use spec::region::Problem;
use types::name::Name;

use crate::pointer::Pointer;

/// The spec that a node uses. A committed pointer takes effect on a node only when its
/// store holds each chunk and the spec has no problem at its build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    /// The pointer of the spec in use, or `None` when the node uses no spec: at the
    /// open, it could not use the spec that it used last, or its founding spec when it
    /// used none, and no later spec took effect.
    pub pointer: Option<Pointer>,
    /// The definitions of the spec in use, by tree key.
    pub definitions: Rc<BTreeMap<Name, Definition>>,
    /// Why the node does not use the newest committed pointer whose read ended, or
    /// `None` when it uses that pointer. A newer pointer can wait for its read, and a
    /// retry keeps the cause of the read before it.
    pub behind: Option<Behind>,
}

/// Why a node does not use the newest committed pointer whose read ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Behind {
    /// The newest committed pointer whose read ended. Its spec is not in use.
    pub pointer: Pointer,
    /// The cause.
    pub cause: Cause,
}

/// Why a node cannot use the spec of a pointer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cause {
    /// The tree does not read: the store lacks a chunk, a chunk does not fit in a
    /// tree, a value is not a definition, or the tree is not the tree of its
    /// definitions.
    Read(spec::region::Error),
    /// The spec has problems at this build, in the order that
    /// [`spec::region::check`] gives them.
    Problems(Vec<Problem>),
    /// A call of the store failed.
    Blob(blob::Error),
    /// The file that records the pointer in use was not made durable.
    Files(files::Error),
}
