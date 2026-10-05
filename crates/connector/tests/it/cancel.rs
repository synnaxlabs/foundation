use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

use connector::cancel::Token;
use types::time::Span;

fn shard() -> env::shards::Config {
    env::shards::Config {
        name: "shard-0".into(),
        core: Some(0),
    }
}

#[test]
fn a_thread_cancel_wakes_a_shard_task() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let token = Token::new();
    let (waiter, clock) = (token.clone(), node.clock());
    let shard = node.shards().start(shard(), move |tasks| async move {
        let woke = Rc::new(Cell::new(false));
        let seen = Rc::clone(&woke);
        let wait = waiter.child().wait();
        tasks.spawn(async move {
            wait.await;
            seen.set(true);
        });
        clock.sleep(Span::SECOND).await;
        assert!(woke.get(), "the task woke at the cancel");
    });
    let clock = node.clock();
    let thread = node.threads().start("vendor", move || async move {
        clock.sleep(Span::MILLISECOND).await;
        token.cancel();
    });
    sim.run().expect("the run ends");
    shard
        .expect("the shard starts")
        .join()
        .expect("the shard ends");
    thread
        .expect("the thread starts")
        .join()
        .expect("the thread ends");
}

#[test]
fn a_shard_cancel_runs_the_hook_that_unblocks_a_thread() {
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let token = Token::new();
    let unblocked = Arc::new(AtomicBool::new(false));
    let (part, flag) = (token.child(), Arc::clone(&unblocked));
    let thread = node.threads().start("vendor", move || async move {
        let _hook = part.on_cancel(move || flag.store(true, SeqCst));
        part.wait().await;
    });
    let clock = node.clock();
    let shard = node.shards().start(shard(), move |_tasks| async move {
        clock.sleep(Span::MILLISECOND).await;
        token.cancel();
    });
    sim.run().expect("the run ends");
    assert!(unblocked.load(SeqCst), "the hook ran");
    shard
        .expect("the shard starts")
        .join()
        .expect("the shard ends");
    thread
        .expect("the thread starts")
        .join()
        .expect("the thread ends");
}
