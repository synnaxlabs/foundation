use std::time::Duration;

use crate::lab::Lab;

#[test]
#[ignore = "waits on #212"]
fn two_nodes_join_by_ticket_and_agree_on_the_spec() {
    let mut lab = Lab::new(1);
    let cloud = lab.start("cloud", 1 << 30);
    let edge = lab.start("edge", 1 << 30);
    let ticket = lab.ticket(cloud);
    lab.join(edge, ticket);
    lab.apply(cloud, "channel \"site.temp\" { data_type = \"f64\" }");
    lab.run(Duration::from_secs(5));
    for node in [cloud, edge] {
        assert_eq!(lab.members(node), ["cloud", "edge"], "{node:?}");
    }
    assert_eq!(lab.spec(edge), lab.spec(cloud), "spec hash");
}
