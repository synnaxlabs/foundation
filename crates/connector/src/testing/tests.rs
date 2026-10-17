use std::future::poll_fn;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::task::Poll;

use document::Document;
use document::diagnostic::{Code, Diagnostic};
use env::clock::Clock;
use hub::reader::{Mode, Reader};
use types::authority::Authority;
use types::channel;
use types::name::{Name, Selector};
use types::time::Span;

use super::{Reading, Rig, create_status};
use crate::cancel;
use crate::common::{env, run_on};
use crate::kind::{Channels, Context, Error, Kind, Table};
use crate::status::State;

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
        |mut rig, _| async move {
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
        |mut rig, _| async move {
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
        |mut rig, _| async move {
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
    on_rig(Table::new, |mut rig, _| async move {
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
    on_rig(Table::new, |mut rig, _| async move {
        let a = name("plant.a");
        rig.define(create_status(&a, &[], channel::Key::from_u128(1)));
        rig.define(create_status(&a, &[], channel::Key::from_u128(10)));
    });
}

#[test]
#[should_panic(expected = "two channels are named plant.ticks.status.time")]
fn supervise_panics_on_a_second_connector_of_one_name() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |mut rig, _| async move {
            let config = Document::default();
            let a = rig
                .supervise("ticks", name("plant.ticks"), config.clone())
                .await;
            assert_eq!(a.stop().await.map_err(|e| e.to_string()), Ok(()));
            let _b = rig.supervise("ticks", name("plant.ticks"), config).await;
        },
    );
}

#[test]
#[should_panic(expected = "two channels are named plant.ticks.status.time")]
fn open_connector_panics_on_a_second_connector_of_one_name() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |mut rig, _| async move {
            let (config, cancel) = (Document::default(), cancel::Token::new());
            let _a = rig
                .supervise("ticks", name("plant.ticks"), config.clone())
                .await;
            let _b = rig
                .open_connector("ticks", name("plant.ticks"), &config, &cancel)
                .await;
        },
    );
}

#[test]
fn two_connectors_of_one_rig_both_run() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |mut rig, _| async move {
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
        |mut rig, _| async move {
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
        |mut rig, clock| async move {
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
#[should_panic(
    expected = "no status within 1s matches, after [Reading { state: Running, restarts: 0"
)]
fn until_panics_with_the_readings_when_none_matches_in_time() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |mut rig, _| async move {
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
        |mut rig, clock| async move {
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
#[should_panic(expected = "a connector whose check failed has no status")]
fn next_panics_on_a_run_whose_check_failed() {
    on_rig(
        || Table::new().with("refuses", Refuses),
        |mut rig, _| async move {
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
        |mut rig, _| async move {
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

/// A run whose status reader sleeps past the room of the ring of its home reads each
/// status frame, with no gap: the first, then one for the counts of each second. Each
/// commit trims the frames before it, so the home applies each frame, and the frames
/// fit the window of the reader.
#[test]
fn a_run_whose_status_reader_sleeps_past_its_ring_reads_each_frame() {
    on_rig(
        || Table::new().with("ticks", Ticks::default()),
        |mut rig, clock| async move {
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
                let received = received.expect("a status frame");
                assert_eq!(received.gap(), None, "frame {taken}");
                taken += 1;
            }
            assert_eq!(taken, 1_101);
        },
    );
}
