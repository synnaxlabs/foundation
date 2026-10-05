use std::time::Duration;

use crate::lab::{Lab, Node};

const HCL: &str = include_str!("fixtures/site.hcl");

#[derive(Clone, Copy)]
enum Front {
    Cli,
    Mcp,
}

fn plan_and_apply(lab: &mut Lab, node: Node, front: Front) -> Vec<String> {
    match front {
        Front::Cli => {
            let changes = lab.plan(node, HCL);
            lab.apply(node, HCL);
            changes
        }
        Front::Mcp => {
            let (plan, changes) = lab.mcp_plan(node, HCL);
            lab.mcp_apply(node, &plan);
            changes
        }
    }
}

/// Checks that a plan of the site names its channels, that the apply built them so a
/// sample written to `site.temp` reads back, and that the next plan has no changes.
fn check(front: Front) {
    let mut lab = Lab::new(1);
    let node = lab.start("cloud");
    let changes = plan_and_apply(&mut lab, node, front);
    assert_eq!(changes, ["site.temp", "site.time"], "first plan");
    lab.write(node, "site.temp", 1, 1);
    lab.run(Duration::from_secs(2));
    assert_eq!(lab.read(node, "admin", "site.temp").samples, 1, "applied");
    let changes = plan_and_apply(&mut lab, node, front);
    assert_eq!(changes, Vec::<String>::new(), "second plan");
    lab.stop();
}

#[test]
#[ignore = "waits on #337"]
fn plan_and_apply_from_hcl_through_the_json_cli() {
    check(Front::Cli);
}

#[test]
#[ignore = "waits on #337"]
fn plan_and_apply_through_mcp() {
    check(Front::Mcp);
}
