use std::future::poll_fn;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::task::Poll;

use document::Document;
use document::diagnostic::{Code, Diagnostic};
use env::clock::Clock;
use hub::home::Outcome;
use hub::reader::{Mode, Reader};
use types::authority::Authority;
use types::channel;
use types::name::{Name, Selector};
use types::time::Span;

use super::{Reading, Rig, create_secret, create_status};
use crate::cancel;
use crate::common::{
    Column, env, fill, open_status, run_on, write_columns, write_status,
};
use crate::kind::{Channels, Context, Error, Kind, Table};
use crate::status::State;

mod secrets;

const REFUSED: Code = Code::new("test.refused");

const TEN: Span = Span::from_nanos(10_000_000_000);

fn name(text: &str) -> Name {
    text.parse().expect("a valid name")
}

/// A kind that adds 1 to its count `ticks` each 100 ms until the cancel. Its run
/// spawns a task that sets `lingered` 2 s after the cancel.
#[derive(Default)]
struct Ticks {
    lingered: Arc<AtomicBool>,
}

impl Kind for Ticks {
    type Config = ();
    const SCHEMA: document::schema::Body = crate::testing::READS;

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels {
            counts: vec![name("ticks")],
            ..Channels::default()
        })
    }

    async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
        let (clock, cancel) = (ctx.clock().clone(), ctx.cancel().clone());
        let lingered = Arc::clone(&self.lingered);
        ctx.tasks().spawn(async move {
            cancel.wait().await;
            clock.sleep(Span::from_nanos(2_000_000_000)).await;
            lingered.store(true, Relaxed);
        });
        let ticks = ctx.count("ticks");
        while ctx
            .cancel()
            .race(ctx.clock().sleep(Span::from_nanos(100_000_000)))
            .await
            .is_some()
        {
            ticks.add(1);
        }
        Ok(())
    }
}

/// A kind whose check refuses each config. Its run ends with `Ok` at once.
struct Refuses;

impl Kind for Refuses {
    type Config = ();
    const SCHEMA: document::schema::Body = crate::testing::READS;

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Err(vec![refused()])
    }

    fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
        std::future::ready(Ok(()))
    }
}

/// A kind whose check passes and whose run ends with `Ok` at once.
struct Ends;

impl Kind for Ends {
    type Config = ();
    const SCHEMA: document::schema::Body = crate::testing::READS;

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    fn run(&self, _: Context<()>) -> impl Future<Output = Result<(), Error>> {
        std::future::ready(Ok(()))
    }
}

fn refused() -> Diagnostic {
    Diagnostic::new(REFUSED, None, "refused".into(), "Fix it".into())
}

/// Opens a rig of `kinds` on a sim node, and runs `test` on it and the node's clock.
fn on_rig<F: Future<Output = ()> + 'static>(
    kinds: impl FnOnce() -> Table + Send + 'static,
    test: impl FnOnce(Rig, Clock) -> F + Send + 'static,
) {
    run_on(move |node, tasks| async move {
        let rig = Rig::open(env(&node, tasks), node.net(), kinds()).await;
        test(rig, node.clock()).await;
    });
}

/// Opens a reader of each status channel of `connector` on the hub of `rig`.
async fn status_of(rig: &Rig, connector: &str, mode: Mode) -> Reader {
    let pattern = format!("{connector}.status.*");
    let config = hub::reader::Config {
        select: Selector::new([pattern.as_str()]).expect("a selector"),
        mode,
        subject: name("plant.test"),
        name: None,
        hold: Span::ZERO,
    };
    rig.hub().reader(config).await.expect("the reader opens")
}

/// The columns of a frame of the statuses `(state, restarts)` of `statuses`, with no
/// error, for [`write_columns`] through a writer of `STATUS_LASTS`.
fn statuses(statuses: &[(u64, u64)]) -> Vec<Column> {
    vec![
        Column::Numbers(statuses.iter().map(|&(state, _)| state).collect()),
        Column::Numbers(vec![0; statuses.len()]),
        Column::Numbers(statuses.iter().map(|&(_, restarts)| restarts).collect()),
    ]
}

/// The status channels of [`statuses`], by the last segment of each name.
const STATUS_LASTS: [&str; 3] = ["state", "class", "restarts"];

/// Asserts that the hub of `rig` defines no channel `channel`.
async fn assert_undefined(rig: &Rig, channel: &str) {
    let config = hub::writer::Config {
        subject: name("plant.test"),
        authority: Authority::ABSOLUTE,
        lease: None,
        channels: vec![name(channel)],
    };
    let Err(error) = rig.hub().writer(config).await else {
        panic!("the hub defines {channel}")
    };
    assert!(matches!(&error, hub::writer::Error::Unknown(n) if *n == name(channel)));
    assert_eq!(error.to_string(), format!("no channel is named {channel}"));
}

/// Whether `reading` is Running with no error, no restart, and a count of ticks above
/// 0.
fn ticking(reading: &super::Reading) -> bool {
    reading.state == State::Running
        && reading.error.is_none()
        && reading.restarts == 0
        && reading
            .counts
            .get(&name("ticks"))
            .is_some_and(|&ticks| ticks > 0)
}

#[test]
fn a_connector_reaches_running_and_stops_with_ok() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, _| async move {
            let config = Document::default();
            let mut run = rig.supervise("ticks", name("plant.ticks"), config).await;
            run.until(TEN, ticking).await;
            assert_eq!(run.stop().await.map_err(|e| e.to_string()), Ok(()));
        },
    );
}

#[test]
fn stop_returns_once_each_task_of_the_run_ended() {
    let ticks = Ticks::default();
    let lingered = Arc::clone(&ticks.lingered);
    on_rig(
        || Table::new().with("ticks", ticks),
        |rig, _| async move {
            let config = Document::default();
            let mut run = rig.supervise("ticks", name("plant.ticks"), config).await;
            run.until(TEN, ticking).await;
            assert_eq!(run.stop().await.map_err(|e| e.to_string()), Ok(()));
            assert!(lingered.load(Relaxed), "the task of the run ended");
        },
    );
}

#[test]
fn a_refused_config_ends_the_run_with_its_diagnostics_and_defines_nothing() {
    on_rig(
        || Table::new().with("refuses", Refuses),
        |rig, _| async move {
            let config = Document::default();
            let run = rig
                .supervise("refuses", name("plant.refuses"), config)
                .await;
            let Err(Error::Config(diagnostics)) = run.stop().await else {
                panic!("a refused config ends with `Config`")
            };
            assert_eq!(diagnostics, [refused()]);
            assert_undefined(&rig, "plant.refuses.status.state").await;
        },
    );
}

#[test]
fn define_keeps_what_an_earlier_call_defined() {
    on_rig(Table::new, |rig, _| async move {
        let (a, b) = (name("plant.a"), name("plant.b"));
        rig.define(create_status(&a, &[], channel::Key::from_u128(1)));
        rig.define(create_status(&b, &[], channel::Key::from_u128(10)));
        let state = name("plant.a.status.state");
        rig.writer(name("plant.test"), vec![state]).await;
    });
}

#[test]
#[should_panic(expected = "two channels are named plant.a.status.time")]
fn define_panics_on_a_name_that_an_earlier_call_defined() {
    on_rig(Table::new, |rig, _| async move {
        let a = name("plant.a");
        rig.define(create_status(&a, &[], channel::Key::from_u128(1)));
        rig.define(create_status(&a, &[], channel::Key::from_u128(10)));
    });
}

#[test]
fn remove_drops_the_channels_of_names_and_keeps_the_rest() {
    on_rig(Table::new, |rig, _| async move {
        rig.define(create_status(
            &name("plant.a"),
            &[],
            channel::Key::from_u128(1),
        ));
        rig.define(create_status(
            &name("plant.b"),
            &[],
            channel::Key::from_u128(10),
        ));
        rig.remove(&[name("plant.a.status.state"), name("plant.other")]);
        assert_undefined(&rig, "plant.a.status.state").await;
        let kept = vec![name("plant.a.status.error"), name("plant.b.status.state")];
        rig.writer(name("plant.test"), kept).await;
        rig.define(create_status(
            &name("plant.c"),
            &[],
            channel::Key::from_u128(20),
        ));
        assert_undefined(&rig, "plant.a.status.state").await;
    });
}

#[test]
fn define_and_remove_keep_the_status_of_a_connector_that_the_rig_opened() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, _| async move {
            let (config, cancel) = (Document::default(), cancel::Token::new());
            let opened = rig
                .open_connector("ticks", name("plant.ticks"), &config, &cancel)
                .await;
            let b = name("plant.b");
            rig.define(create_status(&b, &[], channel::Key::from_u128(1)));
            rig.remove(&[name("plant.b.status.state")]);
            assert_undefined(&rig, "plant.b.status.state").await;
            let state = vec![name("plant.ticks.status.ticks")];
            rig.writer(name("plant.test"), state).await;
            cancel.cancel();
            assert_eq!(opened.run().await.map_err(|e| e.to_string()), Ok(()));
        },
    );
}

/// A kind whose run reads the secret `plant.token` into `read`, then waits for the
/// cancel.
#[derive(Default)]
struct Secret {
    read: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}

impl Kind for Secret {
    type Config = ();
    const SCHEMA: document::schema::Body = crate::testing::READS;

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
        let source = secret::Source::Secret(name("plant.token"));
        let value = ctx.secret(&source).await?;
        let read = value.expose().to_vec();
        self.read
            .lock()
            .expect("no panic under the lock")
            .push(read);
        ctx.cancel().wait().await;
        Ok(())
    }
}

#[test]
fn a_connector_reads_the_secrets_that_create_secret_writes_on_the_files_of_env() {
    let kind = Secret::default();
    let read = Arc::clone(&kind.read);
    run_on(move |node, tasks| async move {
        create_secret(&node.files(), &name("plant.token"), b"t1").await;
        let kinds = Table::new().with("secret", kind);
        let rig = Rig::open(env(&node, tasks), node.net(), kinds).await;
        let config = Document::default();
        let mut run = rig.supervise("secret", name("plant.secret"), config).await;
        run.until(TEN, |reading| reading.state == State::Running)
            .await;
        assert_eq!(run.stop().await.map_err(|e| e.to_string()), Ok(()));
    });
    let read = read.lock().expect("no panic under the lock");
    assert_eq!(*read, [b"t1".to_vec()]);
}

#[test]
fn a_record_reads_each_status_of_a_connector_across_its_runs() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, _| async move {
            let (config, plant) = (Document::default(), name("plant.ticks"));
            let record = rig
                .record(&rig.define_status("ticks", &plant, &config))
                .await;
            for _ in 0..2 {
                let mut run =
                    rig.supervise("ticks", plant.clone(), config.clone()).await;
                run.until(TEN, ticking).await;
                assert_eq!(run.stop().await.map_err(|e| e.to_string()), Ok(()));
            }
            let readings = record.readings().into_iter();
            let got: Vec<_> =
                (readings.map(|r| (r.state, r.counts[&name("ticks")]))).collect();
            let (running, ending) = (State::Running, State::Ending);
            let first = [(running, 0), (running, 10), (ending, 10)];
            let stopped = (State::Stopped, 10);
            let want: Vec<_> =
                first.into_iter().chain([stopped]).chain(first).collect();
            assert_eq!(got, want);
        },
    );
}

#[test]
#[should_panic(
    expected = "the status reader does not open: the selector matches no channel"
)]
fn a_record_of_status_channels_that_the_hub_does_not_define_panics() {
    on_rig(Table::new, |rig, _| async move {
        rig.record(&[name("plant.ticks.status.time")]).await;
    });
}

#[test]
fn a_second_open_of_one_name_reuses_its_status() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, _| async move {
            let (config, plant) = (Document::default(), name("plant.ticks"));
            let names = rig.define_status("ticks", &plant, &config);
            let select = Selector::new(names.iter().map(Name::as_str));
            let mut reader = rig
                .hub()
                .reader(hub::reader::Config {
                    select: select.expect("a selector"),
                    mode: Mode::Complete,
                    subject: name("plant.test"),
                    name: None,
                    hold: Span::ZERO,
                })
                .await
                .expect("the reader opens");
            let mut a = rig.supervise("ticks", plant.clone(), config.clone()).await;
            a.until(TEN, ticking).await;
            assert_eq!(a.stop().await.map_err(|e| e.to_string()), Ok(()));
            let mut b = rig.supervise("ticks", plant.clone(), config.clone()).await;
            b.until(TEN, ticking).await;
            let mut states = Vec::new();
            while states.iter().filter(|s| **s == State::Running).count() < 2 {
                let received = reader.next().await.expect("a status frame");
                let reading = Reading::of(&received).expect("a whole status");
                states.push(reading.state);
            }
            assert_eq!(b.stop().await.map_err(|e| e.to_string()), Ok(()));
            let cancel = cancel::Token::new();
            cancel.cancel();
            let c = rig.open_connector("ticks", plant.clone(), &config, &cancel);
            assert_eq!(c.await.run().await.map_err(|e| e.to_string()), Ok(()));
            assert_eq!(rig.define_status("ticks", &plant, &config), names);
        },
    );
}

#[test]
#[should_panic(
    expected = "a run of plant.ticks goes on: stop it before the next open of its name"
)]
fn a_second_open_after_define_status_while_its_run_goes_on_panics() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, _| async move {
            let (config, plant) = (Document::default(), name("plant.ticks"));
            rig.define_status("ticks", &plant, &config);
            let mut a = rig.supervise("ticks", plant.clone(), config.clone()).await;
            a.until(TEN, ticking).await;
            rig.supervise("ticks", plant, config).await;
        },
    );
}

#[test]
#[should_panic(expected = "the rig holds the status of plant.ticks with the counts \
                           [ticks], and the check of secret gives []")]
fn a_second_open_of_one_name_with_other_counts_panics() {
    on_rig(
        || {
            Table::new()
                .with("ticks", Ticks::default())
                .with("secret", Secret::default())
        },
        |rig, _| async move {
            let config = Document::default();
            let a = rig
                .supervise("ticks", name("plant.ticks"), config.clone())
                .await;
            assert_eq!(a.stop().await.map_err(|e| e.to_string()), Ok(()));
            let _b = rig.supervise("secret", name("plant.ticks"), config).await;
        },
    );
}

#[test]
#[should_panic(expected = "the check of refuses fails: [Diagnostic")]
fn define_status_panics_when_the_check_fails() {
    on_rig(
        || Table::new().with("refuses", Refuses),
        |rig, _| async move {
            let config = Document::default();
            rig.define_status("refuses", &name("plant.refuses"), &config);
        },
    );
}

#[test]
fn a_rig_of_unsynced_starts_its_connectors_once_the_mesh_time_comes() {
    let delay = Span::from_nanos(5_000_000_000);
    run_on(move |node, tasks| async move {
        let kinds = Table::new().with("ticks", Ticks::default());
        let rig = Rig::unsynced(env(&node, tasks), node.net(), kinds, delay).await;
        let (clock, config) = (node.clock(), Document::default());
        let start = clock.now();
        let mut run = rig.supervise("ticks", name("plant.ticks"), config).await;
        run.until(TEN, ticking).await;
        assert!(clock.now() - start >= delay, "{:?}", clock.now() - start);
        assert_eq!(run.stop().await.map_err(|e| e.to_string()), Ok(()));
    });
}

#[test]
fn two_connectors_of_one_rig_both_run() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, _| async move {
            let config = Document::default();
            let mut a = rig
                .supervise("ticks", name("plant.a"), config.clone())
                .await;
            let mut b = rig.supervise("ticks", name("plant.b"), config).await;
            a.until(TEN, ticking).await;
            b.until(TEN, ticking).await;
            assert_eq!(a.stop().await.map_err(|e| e.to_string()), Ok(()));
            assert_eq!(b.stop().await.map_err(|e| e.to_string()), Ok(()));
        },
    );
}

#[test]
fn the_second_connector_of_a_rig_gets_the_status_keys_after_the_first() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, _| async move {
            let config = Document::default();
            let _a = rig
                .supervise("ticks", name("plant.a"), config.clone())
                .await;
            let _b = rig.supervise("ticks", name("plant.b"), config).await;
            let mut reader = status_of(&rig, "plant.b", Mode::Latest).await;
            let received = reader.next().await.expect("a status frame");
            let mut keys: Vec<_> =
                received.set().entries().iter().map(|e| e.key).collect();
            keys.sort_unstable();
            let (_, channels) =
                crate::status::channels(&name("plant.a"), &[name("ticks")])
                    .expect("valid status names");
            let per = u128::try_from(1 + channels.len()).expect("a small count");
            let first = 1_u128 << 64;
            let want: Vec<_> = (first + per..first + 2 * per)
                .map(channel::Key::from_u128)
                .collect();
            assert_eq!(keys, want);
        },
    );
}

#[test]
fn a_run_shows_whether_it_ended() {
    on_rig(
        || Table::new().with("refuses", Refuses),
        |rig, clock| async move {
            let config = Document::default();
            let run = rig
                .supervise("refuses", name("plant.refuses"), config)
                .await;
            assert_eq!(format!("{run:?}"), "Run { ended: false, .. }");
            clock.sleep(Span::SECOND).await;
            assert_eq!(format!("{run:?}"), "Run { ended: true, .. }");
        },
    );
}

#[test]
fn end_gives_the_result_only_of_a_run_that_ended_before_the_call() {
    on_rig(
        || {
            Table::new()
                .with("refuses", Refuses)
                .with("ticks", Ticks::default())
        },
        |rig, _| async move {
            let config = Document::default();
            let refusal = rig
                .supervise("refuses", name("plant.refuses"), config.clone())
                .await;
            let mut ticks = rig.supervise("ticks", name("plant.ticks"), config).await;
            ticks.until(TEN, ticking).await;
            let Some(Err(Error::Config(diagnostics))) = refusal.end().await else {
                panic!("a refused config ends with `Config` before the call")
            };
            assert_eq!(diagnostics, [refused()]);
            let end = ticks.end().await.map(|end| end.map_err(|e| e.to_string()));
            assert_eq!(end, None);
        },
    );
}

#[test]
#[should_panic(
    expected = "a run of plant.ticks goes on: stop it before the next open of its name"
)]
fn a_second_open_of_one_name_while_its_run_goes_on_panics() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, _| async move {
            let config = Document::default();
            let mut a = rig
                .supervise("ticks", name("plant.ticks"), config.clone())
                .await;
            a.until(TEN, ticking).await;
            rig.supervise("ticks", name("plant.ticks"), config).await;
        },
    );
}

#[test]
#[should_panic(
    expected = "a run of plant.ticks goes on: stop it before the next open of its name"
)]
fn a_third_open_while_the_second_run_goes_on_panics() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, _| async move {
            let (config, plant) = (Document::default(), name("plant.ticks"));
            let mut a = rig.supervise("ticks", plant.clone(), config.clone()).await;
            a.until(TEN, ticking).await;
            assert_eq!(a.stop().await.map_err(|e| e.to_string()), Ok(()));
            let mut b = rig.supervise("ticks", plant.clone(), config.clone()).await;
            b.until(TEN, ticking).await;
            rig.supervise("ticks", plant, config).await;
        },
    );
}

#[test]
#[should_panic(
    expected = "a run of plant.ticks goes on: stop it before the next open of its name"
)]
fn an_open_after_a_dropped_run_whose_tasks_go_on_panics() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, _| async move {
            let config = Document::default();
            let mut a = rig
                .supervise("ticks", name("plant.ticks"), config.clone())
                .await;
            a.until(TEN, ticking).await;
            drop(a);
            let cancel = cancel::Token::new();
            let opened =
                rig.open_connector("ticks", name("plant.ticks"), &config, &cancel);
            drop(opened.await);
        },
    );
}

#[test]
#[should_panic(
    expected = "the status reader does not open: the selector matches no channel"
)]
fn an_open_after_a_remove_of_its_status_panics() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, _| async move {
            let (config, plant) = (Document::default(), name("plant.ticks"));
            let names = rig.define_status("ticks", &plant, &config);
            rig.remove(&names);
            rig.supervise("ticks", plant, config).await;
        },
    );
}

#[test]
#[should_panic(
    expected = "no status within 1s matches, after [Reading { state: Running, restarts: 0"
)]
fn until_panics_with_the_readings_when_none_matches_in_time() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, _| async move {
            let config = Document::default();
            let mut run = rig.supervise("ticks", name("plant.ticks"), config).await;
            run.until(Span::SECOND, |reading| reading.restarts > 0)
                .await;
        },
    );
}

/// A drop of a [`super::Run`] cancels its connector.
#[test]
fn a_drop_of_a_run_cancels_the_connector() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, clock| async move {
            let config = Document::default();
            let run = rig.supervise("ticks", name("plant.ticks"), config).await;
            let mut reader = status_of(&rig, "plant.ticks", Mode::Complete).await;
            drop(run);
            let mut stopped = pin!(async {
                loop {
                    let received = reader.next().await.expect("a status frame");
                    let reading = Reading::of(&received);
                    if reading.is_some_and(|reading| reading.state == State::Stopped) {
                        break;
                    }
                }
            });
            let mut late = pin!(clock.sleep(TEN));
            let in_time = poll_fn(|cx| match stopped.as_mut().poll(cx) {
                Poll::Ready(()) => Poll::Ready(true),
                Poll::Pending => late.as_mut().poll(cx).map(|()| false),
            })
            .await;
            assert!(in_time, "the connector stops within 10 s of the drop");
        },
    );
}

#[test]
fn next_gives_each_status_of_a_frame_oldest_first() {
    on_rig(
        || Table::new().with("ends", Ends),
        |rig, clock| async move {
            let config = Document::default();
            let mut run = rig.supervise("ends", name("plant.ends"), config).await;
            run.until(TEN, |reading| reading.state == State::Stopped)
                .await;
            clock.sleep(Span::SECOND).await;
            let columns = STATUS_LASTS.into_iter().zip(statuses(&[(1, 5), (0, 6)]));
            write_status(rig.hub(), "plant.ends", columns.collect()).await;
            let (first, second) = (run.next().await, run.next().await);
            let got = [first, second].map(|reading| (reading.state, reading.restarts));
            assert_eq!(got, [(State::Restarting, 5), (State::Running, 6)]);
        },
    );
}

#[test]
#[should_panic(expected = "the status ends: channel ")]
fn a_record_panics_when_its_status_reader_ends() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, clock| async move {
            let (config, plant) = (Document::default(), name("plant.ticks"));
            let names = rig.define_status("ticks", &plant, &config);
            let _record = rig.record(&names).await;
            rig.remove(&names);
            clock.sleep(Span::SECOND).await;
        },
    );
}

#[test]
fn a_record_keeps_each_status_of_a_frame_oldest_first() {
    on_rig(
        || Table::new().with("ends", Ends),
        |rig, clock| async move {
            let (config, plant) = (Document::default(), name("plant.ends"));
            let names = rig.define_status("ends", &plant, &config);
            let record = rig.record(&names).await;
            let columns = STATUS_LASTS.into_iter().zip(statuses(&[(1, 5), (0, 6)]));
            write_status(rig.hub(), "plant.ends", columns.collect()).await;
            clock.sleep(Span::SECOND).await;
            let got: Vec<_> = record
                .readings()
                .into_iter()
                .map(|reading| (reading.state, reading.restarts))
                .collect();
            assert_eq!(got, [(State::Restarting, 5), (State::Running, 6)]);
        },
    );
}

#[test]
#[should_panic(expected = "a status frame of [plant.ends.status.time, \
    plant.ends.status.class, plant.ends.status.restarts] holds no whole status")]
fn a_record_panics_at_a_frame_with_no_state() {
    on_rig(
        || Table::new().with("ends", Ends),
        |rig, clock| async move {
            let (config, plant) = (Document::default(), name("plant.ends"));
            let names = rig.define_status("ends", &plant, &config);
            let _record = rig.record(&names).await;
            let lasts = &["class", "restarts"];
            let mut writer = open_status(rig.hub(), "plant.ends", lasts).await;
            let columns = vec![Column::Numbers(vec![0]), Column::Numbers(vec![1])];
            write_columns(&mut writer, columns);
            clock.sleep(Span::SECOND).await;
        },
    );
}

#[test]
#[should_panic(expected = "the status reader missed the seqs ")]
fn next_panics_at_a_status_that_its_reader_missed() {
    on_rig(
        || Table::new().with("ends", Ends),
        |rig, clock| async move {
            let config = Document::default();
            let mut run = rig.supervise("ends", name("plant.ends"), config).await;
            run.until(TEN, |reading| reading.state == State::Stopped)
                .await;
            clock.sleep(Span::SECOND).await;
            let lasts = &STATUS_LASTS;
            let mut writer = open_status(rig.hub(), "plant.ends", lasts).await;
            let mut held = fill(&writer);
            drop(held.pop());
            let lost = write_columns(&mut writer, statuses(&[(1, 5)]));
            assert!(matches!(lost[..], [Outcome::Lost { .. }]), "{lost:?}");
            drop(held);
            clock.sleep(Span::SECOND).await;
            write_columns(&mut writer, statuses(&[(0, 6)]));
            run.next().await;
        },
    );
}

#[test]
#[should_panic(expected = "a connector whose check failed has no status")]
fn next_panics_on_a_run_whose_check_failed() {
    on_rig(
        || Table::new().with("refuses", Refuses),
        |rig, _| async move {
            let config = Document::default();
            let mut run = rig
                .supervise("refuses", name("plant.refuses"), config)
                .await;
            run.next().await;
        },
    );
}

#[test]
#[should_panic(expected = "the writer does not open: no channel is named plant.none")]
fn writer_panics_when_the_hub_refuses_it() {
    on_rig(Table::new, |rig, _| async move {
        rig.writer(name("plant.test"), vec![name("plant.none")])
            .await;
    });
}

#[test]
fn a_refused_config_opens_a_connector_that_ends_with_its_diagnostics() {
    on_rig(
        || Table::new().with("refuses", Refuses),
        |rig, _| async move {
            let (config, cancel) = (Document::default(), cancel::Token::new());
            let opened = rig
                .open_connector("refuses", name("plant.refuses"), &config, &cancel)
                .await;
            let Err(Error::Config(diagnostics)) = opened.run().await else {
                panic!("a refused config ends with `Config`")
            };
            assert_eq!(diagnostics, [refused()]);
            assert_undefined(&rig, "plant.refuses.status.state").await;
        },
    );
}

/// A run whose status reader sleeps until the buffer has no room reads its status up to
/// the frames lost for no room, with no end. No write frees the ring, so no frame comes
/// after them.
#[test]
fn a_run_reads_its_status_up_to_the_frames_lost_for_no_room_after_a_long_sleep() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |rig, clock| async move {
            let config = Document::default();
            let mut run = rig.supervise("ticks", name("plant.ticks"), config).await;
            clock.sleep(Span::from_nanos(1_100_000_000_000)).await;
            let reader = run.reader.as_mut().expect("a status reader");
            let mut taken = 0;
            loop {
                let (mut next, mut quiet) =
                    (pin!(reader.next()), pin!(clock.sleep(Span::SECOND)));
                let received = poll_fn(|cx| {
                    if let Poll::Ready(received) = next.as_mut().poll(cx) {
                        return Poll::Ready(Some(received));
                    }
                    quiet.as_mut().poll(cx).map(|()| None)
                })
                .await;
                let Some(received) = received else { break };
                received.expect("a status frame");
                taken += 1;
            }
            assert_eq!(taken, 1_000);
        },
    );
}

#[test]
fn a_capture_takes_the_events_of_its_target_with_the_fields_of_their_parent() {
    let (capture, dispatch) = super::Capture::new("plant");
    tracing::dispatcher::with_default(&dispatch, || {
        let bare = tracing::error_span!("bare");
        let span = tracing::error_span!(
            "run",
            connector = "press",
            gap = tracing::field::Empty
        );
        span.record("gap", 3);
        tracing::warn!(target: "plant", parent: &span, count = 2, "late {}", 1);
        tracing::error!(target: "plant", parent: &bare, "gone");
        tracing::info!(target: "plant", "alone");
        tracing::info!(target: "other", parent: &span, "skipped");
    });
    assert_eq!(
        capture.lines(),
        [
            r#"WARN late 1 connector="press" gap=3 count=2"#,
            "ERROR gone",
            "INFO alone",
        ]
    );
}

#[test]
#[should_panic(expected = "the status reader missed the seqs ")]
fn a_record_panics_at_a_status_that_its_reader_missed() {
    on_rig(
        || Table::new().with("ends", Ends),
        |rig, clock| async move {
            let (config, plant) = (Document::default(), name("plant.ends"));
            let names = rig.define_status("ends", &plant, &config);
            let _record = rig.record(&names).await;
            let lasts = &STATUS_LASTS;
            let mut writer = open_status(rig.hub(), "plant.ends", lasts).await;
            let mut held = fill(&writer);
            drop(held.pop());
            let lost = write_columns(&mut writer, statuses(&[(1, 5)]));
            assert!(matches!(lost[..], [Outcome::Lost { .. }]), "{lost:?}");
            drop(held);
            clock.sleep(Span::SECOND).await;
            write_columns(&mut writer, statuses(&[(0, 6)]));
            clock.sleep(Span::SECOND).await;
        },
    );
}
