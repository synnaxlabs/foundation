use std::time::Duration;

use crate::lab::{Lab, Protocol};

/// Reads point `p` of a simulated device into `dev.p`, then commands point `q` through
/// `dev.q` and checks that the device applied it and the connector acknowledged it.
fn check(protocol: Protocol, hcl: &str) {
    let mut lab = Lab::new(1);
    let edge = lab.start("edge");
    lab.device(edge, protocol, "dev");
    lab.set_point("dev", "p", 21.5);
    lab.apply(edge, hcl);
    lab.run(Duration::from_secs(2));
    let read = lab.samples(edge, "admin", "dev.p");
    assert!(!read.is_empty(), "no samples");
    assert!(
        read.iter().all(|s| s.value.to_bits() == 21.5_f64.to_bits()),
        "read {read:?}"
    );
    assert_eq!(lab.command(edge, "admin", "dev.q", 7.0), Ok(()), "command");
    lab.run(Duration::from_secs(2));
    assert_eq!(lab.point("dev", "q").to_bits(), 7.0_f64.to_bits(), "device");
    let audit = lab.audit(edge, "dev.q");
    assert_eq!(audit.len(), 1, "audit {audit:?}");
    assert_eq!(audit[0].ack, Some(7.0), "ack");
    lab.stop();
}

#[test]
#[ignore = "waits on #274, #435, #1957, #2145"]
fn opc_ua_reads_and_commands() {
    check(Protocol::OpcUa, include_str!("fixtures/opcua.hcl"));
}

#[test]
#[ignore = "waits on #274, #432, #1957, #2145"]
fn modbus_tcp_reads_and_commands() {
    check(Protocol::ModbusTcp, include_str!("fixtures/modbus_tcp.hcl"));
}

#[test]
#[ignore = "waits on #274, #434, #1957, #2145"]
fn modbus_rtu_reads_and_commands() {
    check(Protocol::ModbusRtu, include_str!("fixtures/modbus_rtu.hcl"));
}

#[test]
#[ignore = "waits on #274, #436, #1957, #2145"]
fn ni_daqmx_reads_and_commands() {
    check(Protocol::Ni, include_str!("fixtures/ni.hcl"));
}

#[test]
#[ignore = "waits on #336, #341, #1957, #2145"]
fn influx_receives_every_sample_the_edge_writes() {
    let mut lab = Lab::new(1);
    let cloud = lab.start("cloud");
    let edge = lab.start("edge");
    let ticket = lab.ticket(cloud);
    lab.join(edge, ticket);
    lab.influx(cloud, "influx");
    lab.apply(cloud, include_str!("fixtures/edge.hcl"));
    lab.apply(cloud, include_str!("fixtures/influx.hcl"));
    lab.write(edge, "edge.value", 1000, 5000);
    lab.run(Duration::from_secs(10));
    let stored = lab.stored("influx", "edge.value");
    assert_eq!(stored.samples, 5000, "count");
    assert_eq!(stored.seqs, Some(lab.written("edge.value")), "seqs");
    assert!(stored.contiguous, "contiguous");
    assert_eq!(stored.gaps, [], "gaps");
    lab.stop();
}
