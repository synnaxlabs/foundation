use std::time::Duration;

use crate::lab::{Device, Event, Lab};

/// Reads one point from a simulated `device` into a channel, then commands the same
/// point back and checks that the device applied it and the connector acknowledged it.
fn check(device: Device, hcl: &str) {
    let mut lab = Lab::new(1);
    let edge = lab.start("edge", 1 << 30);
    lab.device(edge, device, "dev");
    lab.set_device_value("dev", "p", 21.5);
    lab.apply(edge, hcl);
    lab.run(Duration::from_secs(2));
    let read = lab.read(edge, "admin", "dev.p");
    assert!(
        read.iter().any(
            |e| matches!(e, Event::Sample(s) if (s.value - 21.5).abs() < f64::EPSILON)
        ),
        "read {read:?}"
    );
    assert_eq!(
        lab.command(edge, "admin", "dev.p_cmd", 7.0),
        Ok(()),
        "command"
    );
    lab.run(Duration::from_secs(2));
    assert!(
        (lab.device_value("dev", "p") - 7.0).abs() < f64::EPSILON,
        "device"
    );
    let audit = lab.audit(edge, "dev.p_cmd");
    assert_eq!(audit.len(), 1, "audit {audit:?}");
    assert_eq!(audit[0].ack, Some(7.0), "ack");
}

#[test]
#[ignore = "waits on #212"]
fn opc_ua_reads_and_commands() {
    check(Device::OpcUa, include_str!("fixtures/opcua.hcl"));
}

#[test]
#[ignore = "waits on #212"]
fn modbus_tcp_reads_and_commands() {
    check(Device::ModbusTcp, include_str!("fixtures/modbus_tcp.hcl"));
}

#[test]
#[ignore = "waits on #212"]
fn modbus_rtu_reads_and_commands() {
    check(Device::ModbusRtu, include_str!("fixtures/modbus_rtu.hcl"));
}

#[test]
#[ignore = "waits on #212"]
fn ni_daqmx_reads_and_commands() {
    check(Device::Ni, include_str!("fixtures/ni.hcl"));
}

#[test]
#[ignore = "waits on #212"]
fn influx_receives_what_the_edge_writes() {
    let mut lab = Lab::new(1);
    let cloud = lab.start("cloud", 1 << 30);
    let edge = lab.start("edge", 1 << 30);
    let ticket = lab.ticket(cloud);
    lab.join(edge, ticket);
    lab.apply(cloud, include_str!("fixtures/store_and_forward.hcl"));
    lab.write(edge, "edge.value", 1000, 5000);
    lab.run(Duration::from_secs(10));
    assert_eq!(
        lab.influx("edge.value"),
        lab.read(cloud, "admin", "edge.value"),
        "same"
    );
}
