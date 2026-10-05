use std::time::Duration;

use crate::lab::Lab;

#[test]
#[ignore = "waits on #295: mesh join"]
fn two_nodes_join_by_ticket_and_agree_on_the_spec() {
    let mut lab = Lab::new(1);
    let cloud = lab.start("cloud");
    let edge = lab.start("edge");
    let ticket = lab.ticket(cloud);
    lab.join(edge, ticket);
    let before = lab.spec(cloud);
    lab.apply(cloud, include_str!("fixtures/site.hcl"));
    lab.run(Duration::from_secs(5));
    for node in [cloud, edge] {
        assert_eq!(lab.members(node), ["cloud", "edge"], "{node:?}");
    }
    assert_ne!(lab.spec(cloud), before, "apply changed the spec");
    assert_eq!(lab.spec(edge), lab.spec(cloud), "edge holds the new spec");
}
