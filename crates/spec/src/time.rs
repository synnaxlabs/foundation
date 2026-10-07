//! The time policy: which peers the nodes it selects use as mesh time references.

use types::name::{Name, Selector};

/// Lists the peer nodes that the nodes `select` matches may use as mesh time
/// references.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    select: Selector,
    peers: Peers,
}

/// The peers of a time policy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Peers {
    /// The voters of the node's region.
    #[default]
    Voters,
    /// These nodes. An empty list is no peer.
    Listed(Vec<Name>),
}

impl Policy {
    /// Makes a policy. The order and repeats of listed peers do not count.
    #[must_use]
    pub fn new(select: Selector, mut peers: Peers) -> Self {
        if let Peers::Listed(names) = &mut peers {
            names.sort();
            names.dedup();
        }
        Self { select, peers }
    }

    /// The nodes the policy applies to.
    #[must_use]
    pub const fn select(&self) -> &Selector {
        &self.select
    }

    /// The peers. Listed peers are in name order with no repeat.
    #[must_use]
    pub const fn peers(&self) -> &Peers {
        &self.peers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select() -> Selector {
        Selector::new(["site_a.**"]).unwrap()
    }

    fn name(text: &str) -> Name {
        text.parse().unwrap()
    }

    #[test]
    fn keeps_the_peers_in_name_order_with_no_repeat() {
        let peers = vec![name("n_3"), name("n_1"), name("n_3")];
        let policy = Policy::new(select(), Peers::Listed(peers));
        assert_eq!(
            policy.peers(),
            &Peers::Listed(vec![name("n_1"), name("n_3")])
        );
        assert_eq!(policy.select(), &select());
    }

    #[test]
    fn tells_no_peer_from_the_region_voters() {
        let none = Policy::new(select(), Peers::Listed(Vec::new()));
        assert_eq!(none.peers(), &Peers::Listed(Vec::new()));
        let voters = Policy::new(select(), Peers::Voters);
        assert_eq!(voters.peers(), &Peers::Voters);
    }
}
