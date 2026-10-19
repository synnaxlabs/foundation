//! The inputs of `supervisor::Config` that a run reads through its `Context`.

use std::sync::{Arc, Mutex};

use connector::cancel;
use connector::http;
use connector::kind::{Channels, Context, Error, Kind, Table};
use connector::testing::Rig;
use document::Document;
use document::diagnostic::Diagnostic;
use types::name::Name;

/// Records the roots and the mesh time of its run.
struct Inputs(Arc<Mutex<Option<(String, i64)>>>);

impl Kind for Inputs {
    type Config = ();
    const SCHEMA: document::schema::Body = connector::testing::READS;

    fn parse(&self, _: &Document) -> Result<(), Vec<Diagnostic>> {
        Ok(())
    }

    fn check(&self, (): &()) -> Result<Channels, Vec<Diagnostic>> {
        Ok(Channels::default())
    }

    fn discover(
        &self,
        _: &cancel::Token,
    ) -> impl Future<Output = Result<Vec<Document>, Error>> {
        std::future::ready(Ok(Vec::new()))
    }

    async fn run(&self, ctx: Context<()>) -> Result<(), Error> {
        let roots = format!("{:?}", ctx.roots());
        let now = ctx.time().await.now().nanos();
        *self.0.lock().unwrap() = Some((roots, now));
        Ok(())
    }
}

#[test]
fn a_run_reads_the_roots_and_the_mesh_time_of_its_supervisor() {
    let got = Arc::new(Mutex::new(None));
    let into = Arc::clone(&got);
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let result = sim.run_on(&node, move |node, tasks| async move {
        let env = hub::testing::Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks: tasks.clone(),
        };
        let kinds = Table::new().with("inputs", Inputs(into));
        let rig = Rig::trusting(env, node.net(), kinds, http::sim::ca()).await;
        let first = rig.hub().time().await.now();
        let connector: Name = "plant.inputs".parse().unwrap();
        let (token, document) = (cancel::Token::new(), Document::default());
        let opened = rig.open_connector("inputs", connector, &document, &token);
        opened.await.run().await.map(|()| first)
    });
    let first = result.expect("the sim runs").expect("the run ends");
    let (roots, now) = got.lock().unwrap().clone().expect("the run recorded");
    assert_eq!(roots, "Roots { count: 1 }");
    assert!(now >= first.nanos(), "{now} is before {first:?}");
}

#[test]
fn a_run_of_an_opened_rig_trusts_no_roots() {
    let got = Arc::new(Mutex::new(None));
    let into = Arc::clone(&got);
    let mut sim = sim::Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let result = sim.run_on(&node, move |node, tasks| async move {
        let env = hub::testing::Env {
            files: node.files(),
            clock: node.clock(),
            wall: node.wall(),
            entropy: node.entropy(),
            tasks: tasks.clone(),
        };
        let kinds = Table::new().with("inputs", Inputs(into));
        let rig = Rig::open(env, node.net(), kinds).await;
        let connector: Name = "plant.inputs".parse().unwrap();
        let (token, document) = (cancel::Token::new(), Document::default());
        let opened = rig.open_connector("inputs", connector, &document, &token);
        opened.await.run().await
    });
    result.expect("the sim runs").expect("the run ends");
    let (roots, _) = got.lock().unwrap().clone().expect("the run recorded");
    assert_eq!(roots, "Roots { count: 0 }");
}
