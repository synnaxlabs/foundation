//! The output of `foundation status --json`.

use std::collections::BTreeMap;

/// One connector in `foundation status --json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Connector {
    pub(crate) kind: String,
    /// `running`, `restarting`, `stopped`, or `unknown`.
    pub(crate) state: String,
    /// The `address` attribute of the config, or `None` when it has none.
    pub(crate) address: Option<String>,
    /// `None` while the state is `unknown`.
    pub(crate) restarts: Option<u64>,
    /// Each count that the kind names, such as `in` and `confirmed`. `None` while the
    /// state is `unknown`.
    pub(crate) counts: Option<BTreeMap<String, u64>>,
    /// The last error. `None` when the connector has none, or while the state is
    /// `unknown`.
    pub(crate) error: Option<Error>,
}

/// The last error of a connector in `foundation status --json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Error {
    /// `config`, `device`, or `retry`.
    pub(crate) class: String,
    pub(crate) text: String,
    /// The time of the next try. `None` unless the state is `restarting`.
    pub(crate) next: Option<String>,
}
