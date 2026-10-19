use std::cell::{Cell, RefCell};
use std::future::poll_fn;
use std::pin::pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use document::Document;
use document::diagnostic::Diagnostic;
use secret::Source;
use types::name::Name;
use types::time::Span;

use crate::cancel;
use crate::common::{env, run_in};
use crate::kind::{Channels, Context, Error, Kind, Table};
use crate::testing::{Rig, create_secret, create_secrets};

/// Each value that reads of the secret `plant.token` give while a test writes `t1`
/// to it, and then `t2`.
fn read_during_write(seed: u64) -> Vec<Vec<u8>> {
    run_in(seed, |node, tasks| async move {
        let name: Name = "plant.token".parse().unwrap();
        let (files, clock) = (node.files(), node.clock());
        let secrets = create_secrets(files.clone());
        let got = Rc::new(RefCell::new(Vec::new()));
        let done = Rc::new(Cell::new(false));
        let (into, ended) = (Rc::clone(&got), Rc::clone(&done));
        let read = Source::Secret(name.clone());
        tasks.spawn(async move {
            loop {
                let last = ended.get();
                if let Ok(value) = secrets.get(&read).await {
                    into.borrow_mut().push(value.expose().to_vec());
                }
                if last {
                    break;
                }
                clock.sleep(Span::from_nanos(500)).await;
            }
        });
        create_secret(&files, &name, b"t1").await;
        create_secret(&files, &name, b"t2").await;
        done.set(true);
        node.clock().sleep(Span::SECOND).await;
        got.take()
    })
}

#[test]
fn a_read_gets_no_value_or_a_whole_written_value() {
    for seed in 0..64 {
        let values: Vec<_> = (read_during_write(seed).into_iter())
            .map(|value| String::from_utf8(value).unwrap())
            .collect();
        let whole = values.iter().all(|value| value == "t1" || value == "t2");
        assert!(whole, "seed {seed}: {values:?}");
        assert_eq!(values.last().map(String::as_str), Some("t2"), "seed {seed}");
    }
}

/// The values of `names` after two tasks write them at once.
fn two_at_once(seed: u64, names: [&'static str; 2]) -> [Option<Vec<u8>>; 2] {
    run_in(seed, move |node, tasks| async move {
        let files = node.files();
        let other = files.clone();
        tasks.spawn(async move {
            create_secret(&other, &names[0].parse().unwrap(), b"aaaa").await;
        });
        create_secret(&files, &names[1].parse().unwrap(), b"bb").await;
        node.clock().sleep(Span::SECOND).await;
        let secrets = create_secrets(files);
        let mut got = [None, None];
        for (value, key) in got.iter_mut().zip(names) {
            let read = secrets.get(&Source::Secret(key.parse().unwrap())).await;
            *value = read.ok().map(|value| value.expose().to_vec());
        }
        got
    })
}

#[test]
fn two_secrets_written_at_once_both_land() {
    // Two names that a staged file named by a hash of the name would put in one file.
    let pairs = [
        ["plant.a", "plant.b"],
        ["p.ac5d98e62022af56d", "p.a8f75ad3ac7923eff"],
    ];
    for (names, seed) in pairs
        .into_iter()
        .flat_map(|names| (0..32).map(move |seed| (names, seed)))
    {
        let got = two_at_once(seed, names);
        let want = [Some(b"aaaa".to_vec()), Some(b"bb".to_vec())];
        assert_eq!(got, want, "{names:?}, seed {seed}");
    }
}

/// The value of `plant.a` after a write of it that ends at `stop` and a write of
/// `t2`.
fn write_after_a_dropped_write(seed: u64, stop: i64) -> Option<Vec<u8>> {
    run_in(seed, move |node, _| async move {
        let (files, clock) = (node.files(), node.clock());
        let name: Name = "plant.a".parse().unwrap();
        {
            let mut write = pin!(create_secret(&files, &name, b"long-value"));
            let mut stop = pin!(clock.sleep(Span::from_nanos(stop)));
            poll_fn(|cx| {
                if write.as_mut().poll(cx).is_ready()
                    || stop.as_mut().poll(cx).is_ready()
                {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
        }
        clock.sleep(Span::SECOND).await;
        create_secret(&files, &name, b"t2").await;
        let read = Source::Secret(name.clone());
        let value = create_secrets(files.clone()).get(&read).await;
        value.ok().map(|value| value.expose().to_vec())
    })
}

#[test]
fn a_write_after_a_dropped_write_lands() {
    for seed in 0..32 {
        for stop in [1, 1_000, 10_000, 100_000, 1_000_000, 5_000_000] {
            let got = write_after_a_dropped_write(seed, stop);
            assert_eq!(got, Some(b"t2".to_vec()), "seed {seed} stop {stop}");
        }
    }
}

/// Reads `plant.token` until the store gives it, and records the value.
struct Watch(Arc<Mutex<Vec<Vec<u8>>>>);

impl Kind for Watch {
    type Config = ();
    const SCHEMA: document::schema::Body = crate::testing::READS;

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
        let source = Source::Secret("plant.token".parse().unwrap());
        loop {
            match ctx.secret(&source).await {
                Ok(value) => {
                    self.0.lock().unwrap().push(value.expose().to_vec());
                    return Ok(());
                }
                Err(Error::Retry(_)) => {
                    ctx.clock().sleep(Span::from_nanos(1_000)).await;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

/// The values that a supervised connector reads of `plant.token` while a test
/// writes `t1` to it.
fn read_in_a_run(seed: u64) -> Vec<Vec<u8>> {
    let got = Arc::new(Mutex::new(Vec::new()));
    let into = Arc::clone(&got);
    let result = run_in(seed, move |node, tasks| async move {
        let kinds = Table::new().with("watch", Watch(into));
        let rig = Rig::open(env(&node, tasks.clone()), node.net(), kinds).await;
        let connector: Name = "plant.watch".parse().unwrap();
        let (files, clock) = (node.files(), node.clock());
        tasks.spawn(async move {
            clock.sleep(Span::from_nanos(50_000)).await;
            create_secret(&files, &"plant.token".parse().unwrap(), b"t1").await;
        });
        let (token, document) = (cancel::Token::new(), Document::default());
        let opened = rig.open_connector("watch", connector, &document, &token);
        opened.await.run().await
    });
    result.expect("the run ends with the value");
    got.lock().unwrap().clone()
}

#[test]
fn a_supervised_connector_reads_only_the_written_secret() {
    let torn: Vec<_> = (0..64)
        .map(|seed| (seed, read_in_a_run(seed)))
        .filter(|(_, got)| got != &[b"t1".to_vec()])
        .collect();
    assert_eq!(torn, [], "{} of 64 seeds read another value", torn.len());
}
