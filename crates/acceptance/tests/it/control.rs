use std::time::Duration;

use crate::lab::{Device, Lab};

#[test]
#[ignore = "waits on #212"]
fn a_subject_without_authority_cannot_command_and_the_audit_records_who_did() {
    let mut lab = Lab::new(1);
    let edge = lab.start("edge", 1 << 30);
    lab.device(edge, Device::ModbusTcp, "dev");
    lab.apply(edge, include_str!("fixtures/modbus_tcp.hcl"));
    lab.apply(edge, include_str!("fixtures/control.hcl"));
    lab.run(Duration::from_secs(1));
    assert_eq!(
        lab.command(edge, "viewer", "dev.p_cmd", 3.0),
        Err("viewer may not write dev.p_cmd".to_string()),
        "denied"
    );
    assert_eq!(
        lab.command(edge, "operator", "dev.p_cmd", 4.0),
        Ok(()),
        "allowed"
    );
    lab.run(Duration::from_secs(1));
    let audit = lab.audit(edge, "dev.p_cmd");
    assert_eq!(audit.len(), 1, "audit {audit:?}");
    assert_eq!(audit[0].subject, "operator", "subject");
    assert_eq!(
        (audit[0].value, audit[0].ack),
        (4.0, Some(4.0)),
        "value and ack"
    );
}
