use std::time::Duration;

use crate::lab::{Lab, Protocol};

#[test]
#[ignore = "waits on #338"]
fn a_subject_without_authority_cannot_command_and_the_audit_records_who_did() {
    let mut lab = Lab::new(1);
    let edge = lab.start("edge");
    lab.device(edge, Protocol::ModbusTcp, "dev");
    lab.apply(edge, include_str!("fixtures/modbus_tcp.hcl"));
    lab.apply(edge, include_str!("fixtures/control.hcl"));
    lab.run(Duration::from_secs(1));
    assert_eq!(
        lab.command(edge, "viewer", "dev.q", 3.0),
        Err("viewer may not write dev.q".to_string()),
        "denied"
    );
    lab.run(Duration::from_secs(1));
    assert_eq!(
        lab.point("dev", "q").to_bits(),
        0.0_f64.to_bits(),
        "device untouched"
    );
    assert_eq!(
        lab.command(edge, "operator", "dev.q", 4.0),
        Ok(()),
        "allowed"
    );
    lab.run(Duration::from_secs(1));
    let audit = lab.audit(edge, "dev.q");
    assert_eq!(audit.len(), 1, "audit {audit:?}");
    assert_eq!(audit[0].subject, "operator", "subject");
    assert_eq!(
        (audit[0].value, audit[0].ack),
        (4.0, Some(4.0)),
        "value and ack"
    );
    lab.stop();
}
