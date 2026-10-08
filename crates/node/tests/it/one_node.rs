//! The exit tests of ONE NODE: one `foundation` node reads a simulated OPC UA server
//! and pushes each sample to a simulated InfluxDB.

use std::collections::BTreeMap;

use types::quality::Quality;
use types::time::Span;

use crate::rig::{Connector, Error, Rig, Sample};
use crate::text;

/// The config of the quickstart page.
const PLANT: &str = include_str!("one_node/plant.hcl");

/// The channel `plant.spike`, with the quality channel `plant.quality`.
const QUALITY: &str = include_str!("one_node/quality.hcl");

/// Each variable of the OPC UA server, and the channel that reads it in each config.
const VARIABLES: [(&str, &str); 3] = [
    ("ns=3;s=SpikeData", "plant.spike"),
    ("ns=3;s=DipData", "plant.dip"),
    ("ns=3;s=PositiveTrendData", "plant.trend"),
];

/// The changes of each variable: 5 s at 10 Hz.
const TICKS: u32 = 50;

/// The MVP target of the time error bound.
const BOUND: Span = Span::SECOND;

/// `Bad`, with no subcode.
const BAD: Quality = Quality(0x8000_0000);

/// A node on `hcl` that reads the OPC UA server and pushes to InfluxDB.
fn running(hcl: &str) -> Rig {
    let mut rig = Rig::new();
    rig.opcua.serve();
    rig.influx.serve();
    rig.config(hcl);
    rig.start();
    rig.apply();
    rig
}

/// What the server served of one variable, and what InfluxDB holds of its channel.
#[derive(Debug)]
struct Trace {
    channel: &'static str,
    served: Vec<Sample>,
    stored: Vec<Sample>,
}

impl Trace {
    /// Asserts that InfluxDB holds the value of each served sample, in order, each at a
    /// time within [`BOUND`] of the time it was served.
    fn assert_held(&self) {
        let values = |samples: &[Sample]| -> Vec<u64> {
            samples
                .iter()
                .map(|sample| sample.value.to_bits())
                .collect()
        };
        let channel = self.channel;
        assert_eq!(
            values(&self.stored),
            values(&self.served),
            "{channel}: the values in order"
        );
        for (served, stored) in self.served.iter().zip(&self.stored) {
            assert!(
                (stored.time - served.time).nanos().abs() <= BOUND.nanos(),
                "{channel}: {stored:?} is more than {BOUND} from {served:?}"
            );
        }
    }
}

/// The trace of each variable of `variables`, once InfluxDB holds as many samples of
/// each as the server served. Else `Err` with the counts.
fn caught_up(
    rig: &Rig,
    variables: &[(&str, &'static str)],
) -> Result<Vec<Trace>, String> {
    let mut served = rig.opcua.served();
    let mut stored = rig.influx.stored();
    let traces: Vec<_> = variables
        .iter()
        .map(|&(variable, channel)| Trace {
            channel,
            served: served.remove(variable).unwrap_or_default(),
            stored: stored.remove(channel).unwrap_or_default(),
        })
        .collect();
    if traces
        .iter()
        .all(|trace| trace.served.len() == trace.stored.len())
    {
        return Ok(traces);
    }
    let counts: Vec<_> = traces
        .iter()
        .map(|trace| (trace.channel, trace.served.len(), trace.stored.len()))
        .collect();
    Err(format!("each channel, served, and stored: {counts:?}"))
}

/// The count `key` of the connector `name` in `status`.
fn count(status: &BTreeMap<String, Connector>, name: &str, key: &str) -> Option<u64> {
    status.get(name)?.counts.get(key).copied()
}

/// `in` of `plc` and `confirmed` of `influx`, once both are above `floor`.
fn counts_above(rig: &Rig, floor: (u64, u64)) -> Result<(u64, u64), String> {
    let status = rig.status();
    match (
        count(&status, "plc", "in"),
        count(&status, "influx", "confirmed"),
    ) {
        (Some(read), Some(confirmed)) if read > floor.0 && confirmed > floor.1 => {
            Ok((read, confirmed))
        }
        _ => Err(format!("{status:#?}")),
    }
}

#[test]
#[ignore = "waits on #120, #337, #435, #1156, #1732, #1734, #1744"]
fn samples_from_an_opc_ua_server_reach_influxdb() {
    let rig = running(PLANT);
    rig.wait("InfluxDB holds the first values", |rig| {
        caught_up(rig, &VARIABLES)
    });
    rig.opcua.change(TICKS, Quality::GOOD);
    for trace in rig.wait("InfluxDB holds each sample", |rig| {
        caught_up(rig, &VARIABLES)
    }) {
        trace.assert_held();
    }
}

#[test]
#[ignore = "waits on #120, #337, #435, #1156, #1732, #1734, #1744"]
fn a_bad_status_keeps_its_quality() {
    let rig = running(QUALITY);
    let spike = &VARIABLES[..1];
    rig.wait("InfluxDB holds the first value", |rig| {
        caught_up(rig, spike)
    });
    rig.opcua.change(TICKS, BAD);
    let traces = rig.wait("InfluxDB holds each sample", |rig| caught_up(rig, spike));
    let [trace] = &traces[..] else {
        panic!("one variable: {traces:?}");
    };
    let qualities = |samples: &[Sample]| -> Vec<Option<Quality>> {
        samples.iter().map(|sample| sample.quality).collect()
    };
    assert_eq!(qualities(&trace.stored), qualities(&trace.served));
}

#[test]
#[ignore = "waits on #337, #435, #1732, #1734, #1744"]
fn a_wrong_config_gives_its_file_line_and_fix() {
    let mut rig = Rig::new();
    let hcl = PLANT
        .replacen("data_type", "datatype", 1)
        .replace("\"http://localhost:8086\"", "8086");
    rig.config(&hcl);
    rig.start();
    let output = rig.run(&["plan", "plant.hcl", "--out", "plant.plan"]);
    assert_eq!(
        (
            output.status.code(),
            text(&output.stdout),
            text(&output.stderr)
        ),
        (
            Some(2),
            "",
            "error[document.unknown-attribute]: `datatype` is not an attribute of the \
             `channel` block\n  \
             --> plant.hcl:6:3\n\
             fix: Use `kind`, `data_type`, `index`, `quality`, or `unit`, or remove it\n\
             \n\
             error[connector.bad-uri]: a URI is a string, not an integer\n  \
             --> plant.hcl:36:17\n\
             fix: Write an `http` URI such as \"http://10.0.0.2:8086\"\n"
        )
    );
    assert!(!rig.dir.join("plant.plan").exists(), "plan writes no plan");
}

#[test]
#[ignore = "waits on #120, #337, #435, #1156, #1732, #1734, #1735, #1744"]
fn a_wrong_endpoint_shows_in_status_and_heals() {
    let mut rig = Rig::new();
    rig.opcua.serve();
    let influx = rig.influx.serve();
    rig.influx.stop();
    rig.config(PLANT);
    rig.start();
    rig.apply();
    let restarting = rig.wait("`influx` restarts", |rig| {
        let status = rig.status();
        status
            .get("influx")
            .filter(|connector| connector.error.is_some())
            .cloned()
            .ok_or_else(|| format!("{status:#?}"))
    });
    assert_eq!(
        restarting,
        Connector {
            kind: "influx".into(),
            state: "restarting".into(),
            address: format!("http://{influx}"),
            counts: BTreeMap::from([("confirmed".into(), 0)]),
            error: Some(Error {
                class: "device".into(),
                text: format!("the connect failed: {influx} refused the connection"),
            }),
        }
    );
    rig.wait("`plc` reads the first value of each variable", |rig| {
        let status = rig.status();
        match count(&status, "plc", "in") {
            Some(read) if read >= 3 => Ok(read),
            _ => Err(format!("{status:#?}")),
        }
    });
    rig.opcua.change(TICKS, Quality::GOOD);
    rig.influx.serve();
    for trace in rig.wait("InfluxDB holds each sample", |rig| {
        caught_up(rig, &VARIABLES)
    }) {
        trace.assert_held();
    }
}

#[test]
#[ignore = "waits on #120, #337, #435, #1156, #1732, #1734, #1735, #1744"]
fn status_shows_samples_flow() {
    let rig = running(PLANT);
    let before = rig.wait("both counts are above 0", |rig| counts_above(rig, (0, 0)));
    rig.opcua.change(TICKS, Quality::GOOD);
    rig.wait("both counts grow", |rig| counts_above(rig, before));
}
