use crate::lab::Lab;

const HCL: &str = "channel \"site.temp\" { data_type = \"f64\" }";

#[test]
#[ignore = "waits on #212"]
fn plan_and_apply_from_hcl_through_the_json_cli() {
    let mut lab = Lab::new(1);
    let node = lab.start("cloud", 1 << 30);
    let plan = lab.apply(node, HCL);
    assert!(plan.contains("\"site.temp\""), "plan {plan}");
    let again = lab.cli(node, &["plan", "--json"]);
    assert!(again.contains("\"changes\":[]"), "second plan {again}");
}

#[test]
#[ignore = "waits on #212"]
fn plan_and_apply_through_mcp() {
    let mut lab = Lab::new(1);
    let node = lab.start("cloud", 1 << 30);
    let plan = lab.mcp(node, "plan", &format!("{{\"hcl\":{HCL:?}}}"));
    assert!(plan.contains("\"site.temp\""), "plan {plan}");
    let applied = lab.mcp(node, "apply", &plan);
    assert!(applied.contains("\"applied\":true"), "apply {applied}");
}
