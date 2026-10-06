//! The crate map. It must match section 4 of `docs/decisions.md`.

/// One crate in the map.
pub(crate) struct Crate {
    pub name: &'static str,
    pub layer: u8,
    pub deps: Deps,
}

/// The workspace crates a crate may depend on.
pub(crate) enum Deps {
    /// Only these crates.
    Only(&'static [&'static str]),
    /// Any layer 1 crate not in [`TEST_ONLY`], plus these.
    Layer1And(&'static [&'static str]),
    /// Any crate not in [`TEST_ONLY`] in a lower layer or earlier in [`CRATES`].
    Any,
}

/// Crates any crate may use as a dev-dependency, for tests. A normal dependency on
/// one must be named in the crate's [`Deps`].
pub(crate) const TEST_ONLY: &[&str] = &["sim", "counting"];

/// Every crate with its layer and the workspace crates it may depend on.
pub(crate) const CRATES: &[Crate] = &[
    // Layer 1: pure logic. It decides.
    Crate {
        name: "block",
        layer: 1,
        deps: Deps::Only(&[]),
    },
    Crate {
        name: "ring",
        layer: 1,
        deps: Deps::Only(&[]),
    },
    Crate {
        name: "counting",
        layer: 1,
        deps: Deps::Only(&[]),
    },
    Crate {
        name: "types",
        layer: 1,
        deps: Deps::Only(&["block"]),
    },
    Crate {
        name: "env",
        layer: 1,
        deps: Deps::Only(&["types", "block"]),
    },
    Crate {
        name: "document",
        layer: 1,
        deps: Deps::Only(&["types"]),
    },
    Crate {
        name: "raft",
        layer: 1,
        deps: Deps::Only(&["types"]),
    },
    Crate {
        name: "estimate",
        layer: 1,
        deps: Deps::Only(&["types"]),
    },
    Crate {
        name: "control",
        layer: 1,
        deps: Deps::Only(&["types"]),
    },
    Crate {
        name: "delivery",
        layer: 1,
        deps: Deps::Only(&["types", "block"]),
    },
    Crate {
        name: "codec",
        layer: 1,
        deps: Deps::Only(&["types", "block"]),
    },
    Crate {
        name: "wire",
        layer: 1,
        deps: Deps::Only(&["types", "block", "codec"]),
    },
    Crate {
        name: "spec",
        layer: 1,
        deps: Deps::Only(&["types", "document"]),
    },
    Crate {
        name: "access",
        layer: 1,
        deps: Deps::Only(&["types", "spec"]),
    },
    // Layer 2: drives the mesh's disk, network, and clock. It does.
    Crate {
        name: "os",
        layer: 2,
        deps: Deps::Only(&["env", "types", "block"]),
    },
    Crate {
        name: "transport",
        layer: 2,
        deps: Deps::Only(&["env", "types", "block"]),
    },
    Crate {
        name: "buffer",
        layer: 2,
        deps: Deps::Only(&["env", "types", "block", "codec"]),
    },
    Crate {
        name: "clock",
        layer: 2,
        deps: Deps::Only(&["env", "types", "ring", "estimate", "wire", "transport"]),
    },
    Crate {
        name: "blob",
        layer: 2,
        deps: Deps::Only(&["env", "types", "block", "wire", "transport"]),
    },
    Crate {
        name: "sim",
        layer: 2,
        deps: Deps::Only(&["env", "types", "block"]),
    },
    Crate {
        name: "mesh",
        layer: 2,
        deps: Deps::Only(&[
            "env",
            "types",
            "block",
            "raft",
            "spec",
            "access",
            "wire",
            "transport",
            "clock",
            "blob",
        ]),
    },
    Crate {
        name: "home",
        layer: 2,
        deps: Deps::Only(&[
            "env", "types", "block", "ring", "control", "delivery", "codec", "spec",
            "access", "buffer", "clock", "mesh",
        ]),
    },
    Crate {
        name: "replica",
        layer: 2,
        deps: Deps::Only(&[
            "env",
            "types",
            "block",
            "wire",
            "transport",
            "buffer",
            "mesh",
        ]),
    },
    Crate {
        name: "hub",
        layer: 2,
        deps: Deps::Only(&[
            "env",
            "types",
            "block",
            "ring",
            "codec",
            "wire",
            "spec",
            "transport",
            "clock",
            "mesh",
            "home",
        ]),
    },
    // Layer 3: edges. Any layer 1 crate, and only `hub` from layer 2.
    Crate {
        name: "secret",
        layer: 3,
        deps: Deps::Layer1And(&[]),
    },
    Crate {
        name: "connector",
        layer: 3,
        deps: Deps::Layer1And(&["hub", "secret"]),
    },
    // Layer 4: surfaces.
    Crate {
        name: "config-hcl",
        layer: 4,
        deps: Deps::Only(&["types", "document"]),
    },
    Crate {
        name: "config",
        layer: 4,
        deps: Deps::Layer1And(&["connector"]),
    },
    Crate {
        name: "ops",
        layer: 4,
        deps: Deps::Layer1And(&[
            "config",
            "connector",
            "secret",
            "hub",
            "mesh",
            "blob",
            "sim",
        ]),
    },
    Crate {
        name: "node",
        layer: 4,
        deps: Deps::Any,
    },
    Crate {
        name: "acceptance",
        layer: 4,
        deps: Deps::Any,
    },
];

/// Every connector kind crate, named `connector-<kind>`.
const KIND: Crate = Crate {
    name: "connector-<kind>",
    layer: 3,
    deps: Deps::Layer1And(&["hub", "connector"]),
};

/// Finds a crate in the map by package name.
pub(crate) fn find(name: &str) -> Option<&'static Crate> {
    if name.starts_with("connector-") {
        return Some(&KIND);
    }
    CRATES.iter().find(|c| c.name == name)
}

impl Crate {
    /// Reports whether this crate may depend on `dep`.
    pub(crate) fn allows(&self, dep: &str) -> bool {
        match self.deps {
            Deps::Only(list) | Deps::Layer1And(list) if list.contains(&dep) => true,
            Deps::Only(_) => false,
            _ if TEST_ONLY.contains(&dep) => false,
            Deps::Layer1And(_) => find(dep).is_some_and(|d| d.layer == 1),
            Deps::Any => {
                find(dep).is_some_and(|d| d.layer < self.layer || d.earlier(self))
            }
        }
    }

    /// Reports whether this crate comes before `other` in [`CRATES`].
    fn earlier(&self, other: &Crate) -> bool {
        let index = |c: &Crate| CRATES.iter().position(|e| e.name == c.name);
        matches!((index(self), index(other)), (Some(a), Some(b)) if a < b)
    }

    /// Describes the allowed dependencies for an error message.
    pub(crate) fn describe(&self) -> String {
        match self.deps {
            Deps::Only([]) => "no workspace crates".to_string(),
            Deps::Only(list) => list.join(", "),
            Deps::Layer1And([]) => {
                "any layer 1 crate that is not test-only".to_string()
            }
            Deps::Layer1And(list) => format!(
                "any layer 1 crate that is not test-only, {}",
                list.join(", ")
            ),
            Deps::Any => "any earlier crate that is not test-only".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_a_test_only_crate_as_a_normal_dependency_only_where_named() {
        for (name, dep, allowed) in [
            ("secret", "counting", false),
            ("node", "counting", false),
            ("node", "sim", false),
            ("ops", "sim", true),
            ("secret", "block", true),
            ("node", "hub", true),
        ] {
            let entry = find(name).expect("in the map");
            assert_eq!(entry.allows(dep), allowed, "`{name}` on `{dep}`");
        }
    }

    #[test]
    fn allows_any_only_on_crates_earlier_in_the_map() {
        for (name, dep, allowed) in [
            ("node", "acceptance", false),
            ("node", "ops", true),
            ("node", "connector-modbus", true),
            ("acceptance", "node", true),
            ("acceptance", "acceptance", false),
        ] {
            let entry = find(name).expect("in the map");
            assert_eq!(entry.allows(dep), allowed, "`{name}` on `{dep}`");
        }
    }

    #[test]
    fn describes_the_allowed_dependencies() {
        for (name, text) in [
            ("counting", "no workspace crates"),
            ("sim", "env, types, block"),
            ("secret", "any layer 1 crate that is not test-only"),
            (
                "connector",
                "any layer 1 crate that is not test-only, hub, secret",
            ),
            ("node", "any earlier crate that is not test-only"),
        ] {
            let entry = find(name).expect("in the map");
            assert_eq!(entry.describe(), text, "`{name}`");
        }
    }
}
