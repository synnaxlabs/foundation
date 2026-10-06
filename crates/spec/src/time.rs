//! The time policy: which peers the nodes it selects use as mesh time references.

use types::name::{Name, Selector};

/// Lists the peer nodes that the nodes `select` matches may use as mesh time
/// references.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    select: Selector,
    peers: Option<Vec<Name>>,
}

impl Policy {
    /// Makes a policy. `None` means the voters of the node's region, and an empty list
    /// means no peer. The order and repeats of `peers` do not count.
    #[must_use]
    pub fn new(
        select: Selector,
        peers: Option<impl IntoIterator<Item = Name>>,
    ) -> Self {
        let peers = peers.map(|peers| {
            let mut peers = peers.into_iter().collect::<Vec<_>>();
            peers.sort();
            peers.dedup();
            peers
        });
        Self { select, peers }
    }

    /// The nodes the policy applies to.
    #[must_use]
    pub const fn select(&self) -> &Selector {
        &self.select
    }

    /// The peers in name order with no repeat, or `None` for the voters of the node's
    /// region.
    #[must_use]
    pub fn peers(&self) -> Option<&[Name]> {
        self.peers.as_deref()
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
        let peers = [name("n_3"), name("n_1"), name("n_3")];
        let policy = Policy::new(select(), Some(peers));
        assert_eq!(policy.peers(), Some(&[name("n_1"), name("n_3")][..]));
        assert_eq!(policy.select(), &select());
    }

    #[test]
    fn tells_no_peer_from_the_region_voters() {
        let none = Policy::new(select(), Some(Vec::<Name>::new()));
        assert_eq!(none.peers(), Some(&[][..]));
        let voters = Policy::new(select(), None::<[Name; 0]>);
        assert_eq!(voters.peers(), None);
    }
}
