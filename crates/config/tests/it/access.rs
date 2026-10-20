//! The blocks that admit a subject.

use connector::kind::Table;
use types::ed25519::PrivateKey;
use types::name::Name;

#[test]
fn admit_gives_blocks_that_the_spec_check_takes() {
    let subject: Name = "plant.alice".parse().unwrap();
    let public_key = PrivateKey([3; 32]).public();
    let admitted = config::access::admit(&subject, public_key);
    let text = config_hcl::write(&admitted).unwrap();
    assert_eq!(
        text,
        format!(
            "subject \"plant.alice\" {{\n  keys = \"{}\"\n}}\n\naccess \"plant.alice\" \
             {{\n  allow = [\"read\"]\n  select = \"**\"\n  subjects = \
             \"plant.alice\"\n}}\n",
            config::openssh::line(public_key)
        )
    );
    let entries = config::check(&[admitted.into_document()], &Table::new()).unwrap();
    let keys: Vec<_> = entries.keys().map(Name::as_str).collect();
    assert_eq!(keys, ["plant.alice.@access", "plant.alice.@subject"]);
}
