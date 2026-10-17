use std::collections::BTreeMap;
use std::sync::Arc;

use codec::Variable;
use document::diagnostic::{Code, Diagnostic};
use hub::Hub;
use hub::reader::{Config, Mode};
use spec::definition::Definition;
use types::authority::Authority;
use types::frame::{Form, Label, Path};
use types::name::{Name, Selector};
use types::time::Span;

use super::Reading;
use crate::cancel::Token;
use crate::common::{STATUS, env, run_on};
use crate::kind;
use crate::status::{Class, State, Writer};
use crate::testing::create_status;

fn name(text: &str) -> Name {
    text.parse().expect("a valid name")
}

/// What a test does to the status channels.
enum Step {
    Start,
    /// Sets the count `in`, and writes it.
    Count(u64),
    /// Shows the error of this text, and writes it.
    Show(&'static str),
    /// Clears the shown error, and writes it.
    Clear,
    /// Shows the note of this text, and writes it.
    Note(&'static str),
    End(Result<(), kind::Error>),
    Wait,
    Stop,
    /// Closes the status writer, then writes one frame as a program: the samples of
    /// each channel, by the last segment of its name.
    Program(Vec<(&'static str, Column)>),
}

/// The samples of one channel in a frame.
enum Column {
    Numbers(Vec<u64>),
    Texts(Vec<&'static str>),
}

/// The reading of the newest frame of a `Latest` reader of `select`, after a test
/// does `steps` to the status channels of `plant.modbus`, whose kind names the count
/// `in`.
fn read(steps: Vec<Step>, select: &'static [&'static str]) -> Option<Reading> {
    run_on(move |node, tasks| async move {
        let (hub, _) = hub::testing::open(env(&node, tasks)).await;
        let connector = name("plant.modbus");
        let counts = vec![name("in")];
        let definitions: BTreeMap<Name, Definition> =
            create_status(&connector, &counts, STATUS)
                .into_iter()
                .collect();
        hub.set_definitions(definitions.iter());
        let clock = node.clock();
        let opened = Writer::open(
            &hub,
            &connector,
            counts,
            tracing::Span::none(),
            clock.clone(),
            Token::new(),
        );
        let mut open = Some(opened.await.expect("the writer opens"));
        for step in steps {
            if let Step::Program(columns) = step {
                open = None;
                clock.sleep(Span::SECOND).await;
                program(&hub, columns).await;
                continue;
            }
            let (writer, status) = open.as_ref().expect("the status writer is open");
            match step {
                Step::Start => assert!(writer.start().await),
                Step::Count(value) => {
                    status.count("in").set(value);
                    writer.during(clock.sleep(Span::SECOND)).await;
                }
                Step::Show(text) => {
                    status.show(&text);
                    writer.during(clock.sleep(Span::SECOND)).await;
                }
                Step::Clear => {
                    status.clear();
                    writer.during(clock.sleep(Span::SECOND)).await;
                }
                Step::Note(text) => {
                    status.note(&text);
                    writer.during(clock.sleep(Span::SECOND)).await;
                }
                Step::End(end) => writer.end(&end).await,
                Step::Wait => {
                    writer.wait(Span::SECOND).await;
                }
                Step::Stop => writer.stop().await,
                Step::Program(_) => unreachable!("handled above"),
            }
        }
        let config = Config {
            select: Selector::new(select.iter().copied()).expect("a selector"),
            mode: Mode::Latest,
            subject: name("admin"),
            name: None,
            hold: Span::ZERO,
        };
        let mut reader = hub.reader(config).await.expect("the reader opens");
        let received = reader.next().await.expect("the newest frame");
        Reading::of(&received)
    })
}

/// Writes one frame of `columns`, and a stamp of the status index for each sample,
/// as a program.
async fn program(hub: &Hub, columns: Vec<(&'static str, Column)>) {
    let status = |last: &str| name(&format!("plant.modbus.status.{last}"));
    let channels = ["time"]
        .into_iter()
        .chain(columns.iter().map(|(last, _)| *last));
    let config = hub::writer::Config {
        subject: name("program"),
        authority: Authority(1),
        lease: None,
        channels: channels.map(status).collect(),
    };
    let mut program = hub.writer(config).await.expect("the program opens");
    let entries = program.entries().to_vec();
    let set = Arc::clone(program.set());
    let count = match &columns[0].1 {
        Column::Numbers(values) => values.len(),
        Column::Texts(values) => values.len(),
    };
    let start = u64::try_from(program.now().nanos()).expect("a stamp after 1970");
    let stamps = (0..count).map(|i| start + u64::try_from(i).expect("few") * 1_000);
    let stamps = Column::Numbers(stamps.collect());
    let columns = [&stamps]
        .into_iter()
        .chain(columns.iter().map(|(_, column)| column));
    let bytes: Vec<(usize, Vec<u8>)> = entries
        .iter()
        .zip(columns)
        .map(|(&entry, column)| {
            let data_type = set.entries()[entry].data_type;
            let bytes = match column {
                Column::Numbers(values) => {
                    let width = data_type.width().expect("a fixed width");
                    let sample = |value: &u64| {
                        let mut sample = value.to_le_bytes().to_vec();
                        sample.resize(width, 0);
                        sample
                    };
                    values.iter().flat_map(sample).collect()
                }
                Column::Texts(values) => {
                    let form = Variable::of(data_type).expect("a variable type");
                    let len = form.len(values).expect("a short text");
                    let mut bytes = vec![0; len];
                    form.write(values, &mut bytes);
                    bytes
                }
            };
            (entry, bytes)
        })
        .collect();
    let mut series: Vec<_> = bytes
        .iter()
        .map(|(entry, bytes)| (*entry, bytes.len()))
        .collect();
    series.sort_unstable();
    let mut draft = program.draft(Form::Raw, &series).expect("a draft");
    for (entry, bytes) in &bytes {
        let series = draft.series_mut(*entry).expect("a series of the draft");
        series.copy_from_slice(bytes);
    }
    let group = set.entries()[entries[0]].group;
    draft.set_count(group, u32::try_from(count).expect("few"));
    program
        .write(Label::Path(Path::Live), draft)
        .expect("the write goes");
}

/// A reading with the count `in` at `value`.
fn reading(
    state: State,
    restarts: u64,
    error: Option<(Class, &str)>,
    value: u64,
) -> Reading {
    Reading {
        state,
        restarts,
        error: error.map(|(class, text)| (class, text.to_owned())),
        note: None,
        counts: BTreeMap::from([(name("in"), value)]),
    }
}

fn device() -> Step {
    Step::End(Err(kind::Error::Device("busy".into())))
}

const ALL: &[&str] = &["plant.modbus.status.*"];
const NO_BACKOFF: &[&str] = &[ALL[0], "!plant.modbus.status.backoff"];
const NO_RESTARTS: &[&str] = &[ALL[0], "!plant.modbus.status.restarts"];
const NO_ERROR: &[&str] = &[ALL[0], "!plant.modbus.status.error"];
const NO_STATE: &[&str] = &[ALL[0], "!plant.modbus.status.state"];
const NO_CLASS: &[&str] = &[ALL[0], "!plant.modbus.status.class"];

#[test]
fn reads_a_running_connector_and_its_counts() {
    let got = read(vec![Step::Start, Step::Count(7)], ALL);
    assert_eq!(got, Some(reading(State::Running, 0, None, 7)));
}

#[test]
fn reads_the_error_of_a_run_that_ends() {
    let got = read(vec![Step::Start, device()], ALL);
    let want = reading(State::Ending, 0, Some((Class::Device, "busy")), 0);
    assert_eq!(got, Some(want));
}

#[test]
fn reads_a_connector_that_restarts() {
    let got = read(vec![Step::Start, device(), Step::Wait], ALL);
    let want = reading(State::Restarting, 0, Some((Class::Device, "busy")), 0);
    assert_eq!(got, Some(want));
}

#[test]
fn reads_the_restarts_and_keeps_the_last_error_after_a_restart() {
    let got = read(vec![Step::Start, device(), Step::Wait, Step::Start], ALL);
    let want = reading(State::Running, 1, Some((Class::Device, "busy")), 0);
    assert_eq!(got, Some(want));
}

#[test]
fn reads_the_error_that_the_running_kind_shows() {
    let got = read(vec![Step::Start, Step::Show("refused")], ALL);
    let want = reading(State::Running, 0, Some((Class::Retry, "refused")), 0);
    assert_eq!(got, Some(want));
}

#[test]
fn reads_no_error_after_the_running_kind_clears_the_last_one() {
    let steps = vec![Step::Start, device(), Step::Wait, Step::Start, Step::Clear];
    assert_eq!(read(steps, ALL), Some(reading(State::Running, 1, None, 0)));
}

#[test]
fn reads_a_stopped_connector_whose_last_run_returned_ok() {
    let got = read(vec![Step::Start, Step::End(Ok(())), Step::Stop], ALL);
    assert_eq!(got, Some(reading(State::Stopped, 0, None, 0)));
}

#[test]
fn reads_the_class_of_a_config_and_a_retry_error() {
    let bad = Diagnostic::new(
        Code::new("test.bad"),
        None,
        "the rate is bad".into(),
        "Fix it".into(),
    );
    let text = bad.to_string();
    let config = Step::End(Err(kind::Error::Config(vec![bad])));
    let got = read(vec![Step::Start, config], ALL);
    let want = reading(State::Ending, 0, Some((Class::Config, &text)), 0);
    assert_eq!(got, Some(want));
    let retry = Step::End(Err(kind::Error::Retry("again".into())));
    let got = read(vec![Step::Start, retry], ALL);
    let want = reading(State::Ending, 0, Some((Class::Retry, "again")), 0);
    assert_eq!(got, Some(want));
}

#[test]
fn gives_none_for_a_frame_with_no_channel_of_the_supervisor() {
    let got = read(vec![Step::Start], &["plant.modbus.status.in"]);
    assert_eq!(got, None);
}

/// A frame of the supervisor's channels and `in`, as [`columns`] gives them.
fn frame(values: [u64; 5], error: &'static str) -> Step {
    Step::Program(columns(values, error))
}

/// The supervisor's channels and `in`, with one sample of each value, in the order
/// state, class, restarts, backoff, in, and the text of `error`.
fn columns(values: [u64; 5], error: &'static str) -> Vec<(&'static str, Column)> {
    let [state, class, restarts, backoff, count] = values;
    vec![
        ("state", Column::Numbers(vec![state])),
        ("class", Column::Numbers(vec![class])),
        ("restarts", Column::Numbers(vec![restarts])),
        ("backoff", Column::Numbers(vec![backoff])),
        ("in", Column::Numbers(vec![count])),
        ("error", Column::Texts(vec![error])),
    ]
}

#[test]
fn reads_a_frame_that_a_program_writes() {
    let got = read(vec![frame([1, 3, 2, 9, 4], "again")], ALL);
    let want = reading(State::Restarting, 2, Some((Class::Retry, "again")), 4);
    assert_eq!(got, Some(want));
}

#[test]
fn gives_none_for_a_value_that_no_state_has() {
    let got = read(vec![frame([4, 0, 0, 0, 0], "")], ALL);
    assert_eq!(got, None);
}

#[test]
fn gives_none_for_a_value_that_no_class_has() {
    let got = read(vec![frame([0, 4, 0, 0, 0], "busy")], ALL);
    assert_eq!(got, None);
}

#[test]
fn reads_the_newest_of_many_samples() {
    let step = Step::Program(vec![
        ("state", Column::Numbers(vec![0, 2])),
        ("class", Column::Numbers(vec![0, 2])),
        ("restarts", Column::Numbers(vec![0, 3])),
        ("in", Column::Numbers(vec![1, 9])),
        ("error", Column::Texts(vec!["", "busy"])),
    ]);
    let got = read(vec![step], ALL);
    let want = reading(State::Stopped, 3, Some((Class::Device, "busy")), 9);
    assert_eq!(got, Some(want));
}

#[test]
fn reads_a_frame_with_no_backoff() {
    let got = read(vec![frame([2, 0, 1, 0, 6], "")], NO_BACKOFF);
    assert_eq!(got, Some(reading(State::Stopped, 1, None, 6)));
}

#[test]
fn gives_none_for_a_frame_with_no_restarts() {
    let got = read(vec![frame([2, 0, 1, 0, 6], "")], NO_RESTARTS);
    assert_eq!(got, None);
}

#[test]
fn gives_none_for_a_frame_with_no_state() {
    let got = read(vec![frame([2, 0, 1, 0, 6], "")], NO_STATE);
    assert_eq!(got, None);
}

#[test]
fn gives_none_for_a_frame_with_no_class() {
    let got = read(vec![frame([2, 0, 1, 0, 6], "")], NO_CLASS);
    assert_eq!(got, None);
}

#[test]
fn gives_none_for_a_class_with_no_error() {
    let got = read(vec![frame([3, 2, 0, 0, 0], "busy")], NO_ERROR);
    assert_eq!(got, None);
}

#[test]
fn reads_no_error_for_class_0_with_no_error() {
    let got = read(vec![frame([0, 0, 0, 0, 3], "")], NO_ERROR);
    assert_eq!(got, Some(reading(State::Running, 0, None, 3)));
}

#[test]
fn reads_the_note_that_the_running_kind_shows() {
    let got = read(vec![Step::Start, Step::Note("polls")], ALL);
    let want = Reading {
        note: Some("polls".to_owned()),
        ..reading(State::Running, 0, None, 0)
    };
    assert_eq!(got, Some(want));
}

#[test]
fn reads_no_note_after_the_running_kind_shows_an_empty_note() {
    let got = read(vec![Step::Start, Step::Note("polls"), Step::Note("")], ALL);
    assert_eq!(got, Some(reading(State::Running, 0, None, 0)));
}

#[test]
fn reads_no_note_for_an_empty_note() {
    let mut columns = columns([0, 0, 0, 0, 3], "");
    columns.push(("note", Column::Texts(vec![""])));
    let got = read(vec![Step::Program(columns)], ALL);
    assert_eq!(got, Some(reading(State::Running, 0, None, 3)));
}
