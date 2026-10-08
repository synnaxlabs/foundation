//! The home order of an index or a connector: the placement that wins for it, then
//! the node of its connector.

use std::fmt;

use types::name::Name;

use super::Policy;
use crate::definition::Kind;
use crate::resolve::{Tie, resolve};

/// Where one index or connector lives: its home, and the standby and copies of the
/// placement that wins for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placed<'a> {
    /// The tree key of the placement that wins for the name, or `None` when no
    /// placement selects it.
    pub placement: Option<&'a Name>,
    /// The node that orders, buffers, and gates the index, or that runs the connector.
    pub home: &'a Name,
    /// The node that takes over when the home fails.
    pub standby: Option<&'a Name>,
    /// The nodes that keep a copy, in name order with no repeat.
    pub copies: &'a [Name],
}

/// Places `name`, an index or a connector. Of `placements`, the one that selects
/// `name` most specifically wins whole: a less specific one never fills a node it
/// leaves out. The home is the winner's home, else `writer`: the node of the connector
/// that writes the index, or of the connector itself.
///
/// `placements` gives each placement with its tree key. It holds each placement that
/// reaches `name`, once: those of its region and of each region above it. Node
/// names compare as written, so `Edge` and `edge` are two nodes. The caller checks
/// that a node exists, is not reserved, and is in the home's region.
///
/// # Errors
///
/// - [`Unplaced::Tie`] when the two most specific placements tie.
/// - [`Unplaced::NoHome`] when neither the winner nor `writer` gives a home. Only an
///   index gives it: a connector passes its own node as `writer`.
/// - [`Unplaced::Overlap`] when the home comes from `writer` and that node is also the
///   winner's standby or a copy.
pub fn place<'a>(
    name: &Name,
    placements: impl IntoIterator<Item = (&'a Name, &'a Policy)>,
    writer: Option<&'a Name>,
) -> Result<Placed<'a>, Unplaced> {
    let winner = resolve(name, placements, Policy::select)
        .map_err(|Tie { first, second }| Unplaced::Tie { first, second })?;
    let placement = winner.map(|(key, _)| key);
    let policy = winner.map(|(_, policy)| policy);
    let home = match (policy.and_then(Policy::home), writer) {
        (Some(home), _) => home,
        (None, Some(writer)) => {
            if let Some((placement, policy)) = winner
                && (policy.standby() == Some(writer)
                    || policy.copies().contains(writer))
            {
                return Err(Unplaced::Overlap {
                    node: writer.clone(),
                    placement: placement.clone(),
                });
            }
            writer
        }
        (None, None) => {
            return Err(Unplaced::NoHome {
                placement: placement.cloned(),
            });
        }
    };
    Ok(Placed {
        placement,
        home,
        standby: policy.and_then(Policy::standby),
        copies: policy.map_or(&[], Policy::copies),
    })
}

/// An index or a connector that [`place`] cannot place. Its `Display` names each
/// placement by its label, for a message at the label of the name. It calls the name
/// "the name" in `Tie` and `Overlap`, and "the index" in `NoHome`, which only an index
/// gives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unplaced {
    /// The two placements, the first two in name order of those tied, select the name
    /// with the same specificity.
    Tie {
        /// The first of the tied placements, in name order.
        first: Name,
        /// The second of the tied placements, in name order.
        second: Name,
    },
    /// The placement that wins for the index names no home, or no placement selects
    /// it, and no connector writes it.
    NoHome {
        /// The tree key of the placement that wins, or `None` when no placement
        /// selects the index.
        placement: Option<Name>,
    },
    /// The node of the connector is the home and also has a role in the placement.
    Overlap {
        /// The node of the connector.
        node: Name,
        /// The tree key of the placement that wins for the name.
        placement: Name,
    },
}

impl Unplaced {
    /// What to do instead: a sentence with no final period.
    #[must_use]
    pub const fn fix(&self) -> &'static str {
        match self {
            Self::Tie { .. } => {
                "Change the `select` of one of the two placements, so that one selects \
                 the name more specifically"
            }
            Self::NoHome { placement: Some(_) } => {
                "Name a `home` in the placement, or write the index with a connector"
            }
            Self::NoHome { placement: None } => {
                "Select the index with a placement that names a `home`, or write it \
                 with a connector"
            }
            Self::Overlap { .. } => {
                "Move the node to `home` when it is the one node of the placement, else \
                 remove it from the placement"
            }
        }
    }
}

impl fmt::Display for Unplaced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tie { first, second } => write!(
                f,
                "the placements `{}` and `{}` select the name with the same \
                 specificity",
                label(first),
                label(second)
            ),
            Self::NoHome {
                placement: Some(placement),
            } => write!(
                f,
                "the placement `{}` wins for the index and names no home, and no \
                 connector writes the index",
                label(placement)
            ),
            Self::NoHome { placement: None } => write!(
                f,
                "no placement selects the index, and no connector writes it"
            ),
            Self::Overlap { node, placement } => write!(
                f,
                "the node `{node}` of a connector is the home and has another role in \
                 the placement `{}`",
                label(placement)
            ),
        }
    }
}

/// The label of the placement at tree key `key`, or `key` when it has no label form.
fn label(key: &Name) -> Name {
    Kind::Placement.label(key).unwrap_or_else(|| key.clone())
}

impl std::error::Error for Unplaced {}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use types::name::Selector;

    use super::*;
    use crate::placement::Nodes;

    fn name(text: &str) -> Name {
        text.parse().unwrap()
    }

    /// The tree key of the placement labeled `label`.
    fn key(label: &str) -> Name {
        name(&format!("{label}.@placement"))
    }

    fn policy(patterns: &[&str], home: Option<&str>, standby: Option<&str>) -> Policy {
        policy_with_copies(patterns, home, standby, &[])
    }

    fn policy_with_copies(
        patterns: &[&str],
        home: Option<&str>,
        standby: Option<&str>,
        copies: &[&str],
    ) -> Policy {
        let nodes = Nodes {
            home: home.map(name),
            standby: standby.map(name),
            copies: copies.iter().map(|copy| name(copy)).collect(),
        };
        Policy::new(Selector::new(patterns.iter().copied()).unwrap(), nodes).unwrap()
    }

    /// Places `index` with `placements`, each with its tree key, and `writer`.
    fn check<'a>(
        index: &str,
        placements: &'a [(Name, Policy)],
        writer: Option<&'a Name>,
    ) -> Result<Placed<'a>, Unplaced> {
        place(&name(index), placements.iter().map(|(k, p)| (k, p)), writer)
    }

    #[test]
    fn homes_an_index_on_the_home_of_its_placement() {
        let placements = [(key("edge"), policy(&["edge.*"], Some("edge"), None))];
        let placed = check("edge.time", &placements, None).unwrap();
        assert_eq!(placed.placement, Some(&key("edge")));
        assert_eq!(placed.home, &name("edge"));
        assert_eq!(placed.standby, None);
        assert!(placed.copies.is_empty());
    }

    #[test]
    fn prefers_the_home_of_the_placement_to_the_writer() {
        let placements = [(key("p"), policy(&["a.*"], Some("n_1"), Some("n_2")))];
        let writer = name("n_2");
        let placed = check("a.time", &placements, Some(&writer)).unwrap();
        assert_eq!(placed.home, &name("n_1"));
        assert_eq!(placed.standby, Some(&name("n_2")));
    }

    #[test]
    fn homes_an_index_with_no_placement_on_its_writer() {
        let placements = [(key("p"), policy(&["b.*"], Some("n_1"), None))];
        let writer = name("n_2");
        let placed = check("a.time", &placements, Some(&writer)).unwrap();
        assert_eq!(
            placed,
            Placed {
                placement: None,
                home: &writer,
                standby: None,
                copies: &[]
            }
        );
    }

    #[test]
    fn takes_the_winner_whole() {
        let placements = [
            (key("wide"), policy(&["a.**"], Some("n_1"), None)),
            (
                key("narrow"),
                policy_with_copies(&["a.*"], None, Some("n_2"), &["n_3"]),
            ),
        ];
        let writer = name("n_4");
        let placed = check("a.time", &placements, Some(&writer)).unwrap();
        assert_eq!(placed.placement, Some(&key("narrow")));
        assert_eq!(placed.home, &writer, "the wide home does not apply");
        assert_eq!(placed.standby, Some(&name("n_2")));
        assert_eq!(placed.copies, [name("n_3")]);
    }

    #[test]
    fn skips_a_placement_that_excludes_the_index() {
        let placements = [
            (key("wide"), policy(&["a.**"], Some("n_1"), None)),
            (
                key("narrow"),
                policy(&["a.*", "!a.time"], Some("n_2"), None),
            ),
        ];
        let placed = check("a.time", &placements, None).unwrap();
        assert_eq!(placed.placement, Some(&key("wide")));
        assert_eq!(placed.home, &name("n_1"));
    }

    #[test]
    fn refuses_an_index_that_no_placement_selects_and_no_connector_writes() {
        let placements = [(key("p"), policy(&["b.*"], Some("n_1"), None))];
        let none = Unplaced::NoHome { placement: None };
        assert_eq!(check("a.time", &placements, None), Err(none.clone()));
        assert_eq!(
            none.to_string(),
            "no placement selects the index, and no connector writes it"
        );
        assert_eq!(
            none.fix(),
            "Select the index with a placement that names a `home`, or write it with a \
             connector"
        );
    }

    #[test]
    fn refuses_a_winner_with_no_home_that_hides_a_wider_home() {
        let placements = [
            (key("wide"), policy(&["a.**"], Some("n_1"), None)),
            (key("narrow"), policy(&["a.*"], None, Some("n_2"))),
        ];
        let hidden = Unplaced::NoHome {
            placement: Some(key("narrow")),
        };
        assert_eq!(check("a.time", &placements, None), Err(hidden.clone()));
        assert_eq!(
            hidden.to_string(),
            "the placement `narrow` wins for the index and names no home, and no \
             connector writes the index"
        );
        assert_eq!(
            hidden.fix(),
            "Name a `home` in the placement, or write the index with a connector"
        );
    }

    #[test]
    fn refuses_two_placements_with_the_same_specificity() {
        let placements = [
            (key("p_3"), policy(&["*.time"], Some("n_1"), None)),
            (key("p_2"), policy(&["a.*"], Some("n_2"), None)),
            (key("p_1"), policy(&["a.*"], Some("n_3"), None)),
            (key("p_0"), policy(&["**"], Some("n_4"), None)),
        ];
        let tie = Unplaced::Tie {
            first: key("p_1"),
            second: key("p_2"),
        };
        assert_eq!(check("a.time", &placements, None), Err(tie.clone()));
        assert_eq!(
            tie.to_string(),
            "the placements `p_1` and `p_2` select the name with the same specificity"
        );
        let keyless = Unplaced::Tie {
            first: name("p_1"),
            second: key("p_2"),
        };
        assert_eq!(
            keyless.to_string(),
            "the placements `p_1` and `p_2` select the name with the same specificity",
            "a name with no label form shows as given"
        );
        assert_eq!(
            tie.fix(),
            "Change the `select` of one of the two placements, so that one selects the \
             name more specifically"
        );
    }

    #[test]
    fn ties_a_placement_given_twice_with_itself() {
        let placement = policy(&["a.*"], Some("n_1"), None);
        let twice = [(key("p"), placement.clone()), (key("p"), placement)];
        assert_eq!(
            check("a.time", &twice, None),
            Err(Unplaced::Tie {
                first: key("p"),
                second: key("p"),
            })
        );
    }

    #[test]
    fn refuses_a_writer_with_another_role() {
        let standby = [(key("p"), policy(&["a.*"], None, Some("n_1")))];
        let copy = [(
            key("p"),
            policy_with_copies(&["a.*"], None, None, &["n_0", "n_1"]),
        )];
        let writer = name("n_1");
        for placements in [&standby, &copy] {
            assert_eq!(
                check("a.time", placements, Some(&writer)),
                Err(Unplaced::Overlap {
                    node: name("n_1"),
                    placement: key("p"),
                })
            );
        }
        let overlap = Unplaced::Overlap {
            node: name("n_1"),
            placement: key("p"),
        };
        assert_eq!(
            overlap.to_string(),
            "the node `n_1` of a connector is the home and has another role in the \
             placement `p`"
        );
        assert_eq!(
            overlap.fix(),
            "Move the node to `home` when it is the one node of the placement, else \
             remove it from the placement"
        );
    }

    fn pattern() -> impl Strategy<Value = String> {
        prop::collection::vec(prop::sample::select(vec!["a", "b", "*", "**"]), 1..4)
            .prop_map(|segments| segments.join("."))
    }

    proptest! {
        #[test]
        fn picks_the_most_specific_placement(
            index in prop::collection::vec("[ab]", 1..4),
            patterns in prop::collection::vec(pattern(), 0..6),
            homes in prop::collection::vec(any::<bool>(), 6),
        ) {
            let index = name(&index.join("."));
            let placements = patterns
                .iter()
                .enumerate()
                .map(|(i, pattern)| {
                    let label = format!("p_{i}");
                    let home = homes[i].then_some("home");
                    let policy =
                        policy_with_copies(&[pattern.as_str()], home, None, &[&label]);
                    (key(&label), policy)
                })
                .collect::<Vec<_>>();
            let writer = name("writer");
            let pairs = placements.iter().map(|(k, p)| (k, p));
            let got = place(&index, pairs, Some(&writer));
            let matching = placements
                .iter()
                .filter_map(|(k, p)| Some((p.select().matches(&index)?, k, p)))
                .collect::<Vec<_>>();
            let best = matching.iter().map(|(s, ..)| *s).max();
            let mut top = matching
                .iter()
                .filter(|(s, ..)| Some(*s) == best)
                .collect::<Vec<_>>();
            top.sort_by_key(|(_, k, _)| *k);
            match (top.as_slice(), got) {
                ([], Ok(placed)) => {
                    prop_assert_eq!(placed.placement, None);
                    prop_assert_eq!(placed.home, &writer);
                    prop_assert!(placed.copies.is_empty());
                }
                ([(_, key, winner)], Ok(placed)) => {
                    prop_assert_eq!(placed.placement, Some(*key));
                    prop_assert_eq!(placed.copies, winner.copies());
                    prop_assert_eq!(placed.home, winner.home().unwrap_or(&writer));
                }
                (
                    [(_, first, _), (_, second, _), ..],
                    Err(Unplaced::Tie { first: a, second: b }),
                ) => {
                    prop_assert_eq!(&a, *first);
                    prop_assert_eq!(&b, *second);
                }
                (top, got) => prop_assert!(false, "top {top:?} gave {got:?}"),
            }
        }
    }
}
