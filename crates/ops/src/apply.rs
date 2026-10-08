//! Applies a plan file to the spec of the region.

use std::path::Path;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use types::channel;

use crate::error::{self, Error};
use crate::front_end;
use crate::plan::{self, Action, Counts};

#[cfg(test)]
mod tests;

/// Applies the plan in `bytes`, read from the plan file at `path`, to the spec of
/// `mesh`. A new channel gets its key from `key`, as
/// [`config::plan::Plan::definitions`] states.
///
/// # Errors
///
/// - [`Error::Config`] with `ops.path-not-utf8` when `path` is not UTF-8.
/// - [`Error::Plan`] when `bytes` are not a plan, or when the plan holds a change that
///   `plan` does not make from the spec at its base, such as one at a reserved label.
/// - [`Error::Stale`] when the node does not use the spec at the base of the plan, or
///   when another change applies first.
/// - [`Error::Apply`] with each other error of [`mesh::Mesh::apply`], and when the
///   group of `mesh` stopped.
///
/// No error changes the spec. Only `Apply`, and `Stale` when another change applies
/// first, can come after a proposal.
pub(crate) async fn apply(
    path: &Path,
    bytes: &[u8],
    mesh: &mesh::Mesh,
    key: impl FnMut() -> channel::Key,
) -> Result<Applied, Error> {
    let file = path.to_str().ok_or_else(|| {
        Error::Config(vec![plan::problem(front_end::not_utf8(path), &[])])
    })?;
    let planned = config::plan::Plan::decode(bytes).map_err(Error::Plan)?;
    let spec = mesh
        .spec()
        .await
        .map_err(|stopped| Error::Apply(mesh::Error::Stopped(stopped)))?;
    // `definitions` reads the definitions at the base, so the compare comes first.
    if spec.pointer != Some(planned.base) {
        return Err(Error::Stale {
            base: planned.base,
            pointer: spec.pointer,
        });
    }
    let definitions = planned
        .definitions(&spec.definitions, key)
        .map_err(Error::Plan)?;
    let counts = Counts::of(planned.changes.values().map(Action::of));
    let pointer = mesh
        .apply(planned.base, definitions, planned.homes)
        .await
        .map_err(|error| match error {
            mesh::Error::Stale { base, pointer } => Error::Stale {
                base,
                pointer: Some(pointer),
            },
            error => Error::Apply(error),
        })?;
    Ok(Applied {
        file: file.to_owned(),
        pointer: plan::Pointer::from(pointer),
        counts,
    })
}

/// What `apply` changed.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) struct Applied {
    /// The plan file, as the user gave its path.
    pub(crate) file: String,
    /// The spec after the apply.
    pub(crate) pointer: plan::Pointer,
    #[serde(flatten)]
    pub(crate) counts: Counts,
}

impl Applied {
    /// `Applied <file>: <a> added, <c> changed, <r> removed.`, with each count of 0
    /// left out, or `no change` in place of the counts when each is 0.
    pub(crate) fn text(&self) -> String {
        let counts = [
            (self.counts.added, "added"),
            (self.counts.changed, "changed"),
            (self.counts.removed, "removed"),
        ];
        let counts: Vec<String> = counts
            .iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, action)| format!("{count} {action}"))
            .collect();
        let counts = if counts.is_empty() {
            "no change".to_owned()
        } else {
            counts.join(", ")
        };
        format!("Applied {}: {counts}.\n", error::escape(&self.file))
    }
}
