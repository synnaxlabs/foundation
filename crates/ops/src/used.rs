//! The spec that the node uses, which plan and apply start from.

use std::collections::BTreeMap;
use std::rc::Rc;

use spec::definition::Definition;
use types::name::Name;

use crate::error::Error;

/// The pointer and the definitions, by tree key, of the spec that `mesh` uses.
///
/// # Errors
///
/// - [`Error::Behind`] when the node does not use the newest spec.
/// - [`Error::Stopped`] when the group of `mesh` stopped.
pub(crate) async fn spec(
    mesh: &mesh::Mesh,
) -> Result<(spec::Pointer, Rc<BTreeMap<Name, Definition>>), Error> {
    let spec = mesh.spec().await.map_err(Error::Stopped)?;
    if let Some(behind) = spec.behind {
        return Err(Error::Behind(Box::new(behind)));
    }
    let pointer = spec
        .pointer
        .expect("invariant: a node that uses no spec is behind its newest pointer");
    Ok((pointer, spec.definitions))
}
