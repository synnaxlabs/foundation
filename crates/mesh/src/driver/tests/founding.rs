//! Tests of the founding that an open whose log holds no record keeps, and that each
//! open whose log holds a record checks `Config::founding` against.

use env::files::{self, Mode};

use super::*;

use crate::driver::founding::FILE;

/// The founding of node 1 with the members and voters `IDS`.
async fn create_region(node: &sim::node::Node, tasks: &Tasks) -> region::Founding {
    config(node, tasks, 1, &IDS, &IDS).await.founding
}

/// Opens node 1 with `founding`, and drops the mesh.
fn run_with(sim: &mut Sim, node: &sim::node::Node, founding: region::Founding) {
    sim.run_on(node, |node, tasks| async move {
        let config = Config {
            founding,
            ..config(&node, &tasks, 1, &IDS, &IDS).await
        };
        Mesh::start(config).await.unwrap();
    })
    .unwrap();
}

/// Writes a hard state to the log of node 1. One voter of three writes none alone.
fn write_record(sim: &mut Sim, node: &sim::node::Node) {
    sim.run_on(node, |node, _| async move {
        let (mut log, _) = Log::open(node.files(), LOG.into(), create_pool())
            .await
            .unwrap();
        let hard = Hard {
            term: Term(1),
            vote: Some(key(1)),
            leader: None,
            proof: None,
        };
        log.write(Some(hard), &[]).await.unwrap();
    })
    .unwrap();
}

/// A first open with the founding of [`create_region`], and a record in the log.
fn founded(seed: u64) -> (Sim, sim::node::Node, region::Founding) {
    founded_with(seed, BTreeMap::new(), BTreeMap::new())
}

/// [`founded`], with the founding homes `homes`.
fn founded_with(
    seed: u64,
    definitions: BTreeMap<Name, Definition>,
    homes: BTreeMap<channel::Key, node::Key>,
) -> (Sim, sim::node::Node, region::Founding) {
    let mut sim = Sim::new(sim::Config {
        seed,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    let region = sim
        .run_on(&node, |node, tasks| async move {
            region::Founding {
                definitions,
                homes,
                ..create_region(&node, &tasks).await
            }
        })
        .unwrap();
    run_with(&mut sim, &node, region.clone());
    write_record(&mut sim, &node);
    (sim, node, region)
}

/// Opens node 1 with `founding`, and gives the error of the open.
fn refused(sim: &mut Sim, node: &sim::node::Node, founding: region::Founding) -> Error {
    sim.run_on(node, |node, tasks| async move {
        let config = Config {
            founding,
            ..config(&node, &tasks, 1, &IDS, &IDS).await
        };
        Mesh::start(config).await.err().unwrap()
    })
    .unwrap()
}

/// What the log of node 1 holds.
fn logged(sim: &mut Sim, node: &sim::node::Node) -> log::Stored {
    sim.run_on(node, |node, _| async move {
        let (_, stored) = Log::open(node.files(), LOG.into(), create_pool())
            .await
            .unwrap();
        stored
    })
    .unwrap()
}

/// The error of an open with `given` after a first open with `stored`.
fn mismatch(stored: &region::Founding, given: &region::Founding) -> Error {
    Error::Founding {
        stored: Box::new(stored.clone()),
        given: Box::new(given.clone()),
    }
}

#[test]
fn a_reopen_with_fewer_voters_is_refused_before_raft_starts() {
    let (mut sim, node, region) = founded(0);
    let before = logged(&mut sim, &node);
    assert_ne!(before, log::Stored::default());
    let given = region::Founding {
        voters: [key(1)].into(),
        ..region.clone()
    };
    let error = refused(&mut sim, &node, given.clone());
    assert_eq!(error, mismatch(&region, &given));
    let text = format!(
        "the mesh was founded with voters {{{}, {}, {}}}, not {{{}}}",
        key(1),
        key(2),
        key(3),
        key(1)
    );
    assert_eq!(error.to_string(), text);
    assert_eq!(logged(&mut sim, &node), before);
}

#[test]
fn a_reopen_with_another_prefix_is_refused() {
    let (mut sim, node, region) = founded(0);
    let given = region::Founding {
        prefix: Prefix::ROOT,
        ..region.clone()
    };
    let error = refused(&mut sim, &node, given.clone());
    assert_eq!(error, mismatch(&region, &given));
    let text = "the mesh was founded with prefix \"plant\", not \"\"";
    assert_eq!(error.to_string(), text);
}

#[test]
fn a_reopen_with_another_record_of_a_member_is_refused() {
    let (mut sim, node, region) = founded(0);
    let mut given = region.clone();
    given.members = vec![common::member(1), record(2, 2, 9), common::member(3)];
    let error = refused(&mut sim, &node, given.clone());
    assert_eq!(error, mismatch(&region, &given));
    let text = format!(
        "the mesh was founded with another record of member {}",
        key(2)
    );
    assert_eq!(error.to_string(), text);
}

#[test]
fn a_reopen_with_a_member_more_or_less_is_refused() {
    let (mut sim, node, region) = founded(0);
    let less = region::Founding {
        members: common::create_members(&[1, 2]),
        voters: [key(1), key(2)].into(),
        ..region.clone()
    };
    let error = refused(&mut sim, &node, less);
    let text = format!(
        "the mesh was founded with member {}, which the config lacks",
        key(3)
    );
    assert_eq!(error.to_string(), text);
    let more = region::Founding {
        members: common::create_members(&[1, 2, 3, 4]),
        ..region.clone()
    };
    let error = refused(&mut sim, &node, more);
    let text = format!("the mesh was founded with no member {}", key(4));
    assert_eq!(error.to_string(), text);
}

#[test]
fn a_reopen_with_other_definitions_is_refused() {
    let name = |label: &str| spec::definition::Kind::Subject.key(label).unwrap();
    let subject = |id: u8| {
        Definition::Subject(spec::subject::Subject::new(vec![public(id)]).unwrap())
    };
    let stored: BTreeMap<_, _> = [(name("plant.app"), subject(1))].into();
    let (mut sim, node, region) = founded_with(0, stored.clone(), BTreeMap::new());
    let founded = "the mesh was founded with";
    let cases = [
        (
            [(name("plant.app"), subject(2))].into(),
            format!("{founded} another definition plant.app.@subject"),
        ),
        (
            BTreeMap::new(),
            format!("{founded} definition plant.app.@subject, which the config lacks"),
        ),
        (
            [
                (name("plant.app"), subject(1)),
                (name("plant.ops"), subject(1)),
            ]
            .into(),
            format!("{founded} no definition plant.ops.@subject"),
        ),
    ];
    for (definitions, text) in cases {
        let given = region::Founding {
            definitions,
            ..region.clone()
        };
        let error = refused(&mut sim, &node, given.clone());
        assert_eq!(error, mismatch(&region, &given));
        assert_eq!(error.to_string(), text);
    }
}

/// Each text names the index by the tree key of its channel definition, or by its key
/// when no definition has it.
#[test]
fn a_reopen_with_other_homes_is_refused() {
    let homes = |index: u128, home: Option<u8>| {
        let homes = home.map(|id| (common::index(index), key(id)));
        homes.into_iter().collect::<BTreeMap<_, _>>()
    };
    let channel = spec::channel::Channel {
        key: common::index(1),
        kind: spec::channel::Kind::Index {
            error: None,
            control: None,
        },
    };
    let name = spec::definition::Kind::Channel.key("plant.i").unwrap();
    let definitions: BTreeMap<_, _> = [(name, Definition::Channel(channel))].into();
    let (mut sim, node, region) =
        founded_with(0, definitions.clone(), homes(1, Some(2)));
    let cases = [
        (
            Some(3),
            "the mesh was founded with another home of index plant.i",
        ),
        (
            None,
            "the mesh was founded with a home of index plant.i, which the config lacks",
        ),
    ];
    for (home, text) in cases {
        let given = region::Founding {
            homes: homes(1, home),
            ..region.clone()
        };
        let error = refused(&mut sim, &node, given.clone());
        assert_eq!(error, mismatch(&region, &given), "{home:?}");
        assert_eq!(error.to_string(), text);
    }
    let (mut sim, node, region) = founded_with(0, definitions, BTreeMap::new());
    let named = "plant.i".to_owned();
    for (index, name) in [(1, named), (2, common::index(2).to_string())] {
        let given = region::Founding {
            homes: homes(index, Some(2)),
            ..region.clone()
        };
        let error = refused(&mut sim, &node, given.clone());
        assert_eq!(error, mismatch(&region, &given), "index {index}");
        let text = format!("the mesh was founded with no home of index {name}");
        assert_eq!(error.to_string(), text);
    }
    run_with(&mut sim, &node, region);
}

/// A failed file call on the founding file, at an open whose log holds no record and
/// at one whose log holds a record.
#[test]
fn a_failed_call_on_the_founding_file_gives_the_files_error() {
    let failed = |sim: &mut Sim, node: &sim::node::Node, region| {
        let error = refused(sim, node, region);
        let Error::Files(error) = error else {
            panic!("{error:?}");
        };
        error
    };
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let region = sim
        .run_on(&node, |node, tasks| async move {
            create_region(&node, &tasks).await
        })
        .unwrap();
    node.fail_file(Path::new("founding.new"), Operation::WriteAt);
    let error = failed(&mut sim, &node, region);
    let path = PathBuf::from("founding.new");
    let operation = Operation::WriteAt;
    assert_eq!(
        error,
        files::Error::Io {
            path,
            operation,
            code: 5
        }
    );
    let (mut sim, node, region) = founded(0);
    node.fail_file(Path::new(FILE), Operation::ReadAt);
    let error = failed(&mut sim, &node, region);
    let path = PathBuf::from(FILE);
    let operation = Operation::ReadAt;
    assert_eq!(
        error,
        files::Error::Io {
            path,
            operation,
            code: 5
        }
    );
}

#[test]
fn a_reopen_with_the_same_founding_opens_whatever_the_order_of_its_members() {
    let (mut sim, node, mut region) = founded(0);
    region.members.reverse();
    run_with(&mut sim, &node, region);
}

#[test]
fn a_founding_with_definitions_opens_again() {
    let mut sim = Sim::new(sim::Config::default());
    let node = sim.node(sim::node::Config::default());
    let region = sim
        .run_on(&node, |node, tasks| async move {
            region::Founding {
                definitions: super::create_founding(),
                ..create_region(&node, &tasks).await
            }
        })
        .unwrap();
    run_with(&mut sim, &node, region.clone());
    run_with(&mut sim, &node, region);
}

#[test]
fn a_log_with_no_founding_is_refused() {
    let (mut sim, node, region) = founded(0);
    sim.run_on(&node, |node, _| async move {
        node.files().remove(Path::new(FILE)).await.unwrap();
    })
    .unwrap();
    let error = refused(&mut sim, &node, region);
    let path = PathBuf::from(FILE);
    assert_eq!(error, Error::Unfounded { path });
    let text = "the log of the mesh directory holds a record, but founding is not \
                there or does not read back whole";
    assert_eq!(error.to_string(), text);
}

/// Flips the bytes of the check, of the version, and of the start and end of the
/// body, one at a time.
#[test]
fn a_founding_with_a_flipped_byte_is_refused() {
    let (mut sim, node, region) = founded(0);
    let flip = |sim: &mut Sim, at: u64| {
        sim.run_on(&node, move |node, _| async move {
            let path = Path::new(FILE);
            let file = node.files().open(path, Mode::Write).await.unwrap();
            let pool = create_pool();
            let byte = file.read_at(at, pool.alloc(1).unwrap()).await.unwrap();
            let flipped = byte.first().unwrap() ^ 1;
            let block = crate::bytes::block(&pool, &[flipped]).unwrap();
            file.write_at(at, &[block]).await.unwrap();
            file.sync().await.unwrap();
            file.len()
        })
        .unwrap()
    };
    let len = flip(&mut sim, 0);
    flip(&mut sim, 0);
    let last = len.checked_sub(1).unwrap();
    for at in (0..16).chain([last]) {
        flip(&mut sim, at);
        let error = refused(&mut sim, &node, region.clone());
        let path = PathBuf::from(FILE);
        assert_eq!(error, Error::Unfounded { path }, "byte {at}");
        flip(&mut sim, at);
    }
    run_with(&mut sim, &node, region);
}

/// A founding whose check holds, with another version or a body that does not read.
#[test]
fn a_founding_of_another_version_or_form_is_refused() {
    let forms = [(2_u16, vec![0]), (1, vec![]), (1, vec![9; 40])];
    for (version, body) in forms {
        let (mut sim, node, region) = founded(0);
        sim.run_on(&node, move |node, _| async move {
            let mut rest = version.to_le_bytes().to_vec();
            rest.extend(body);
            let check = types::digest::Digest::of(&rest).0;
            let mut bytes = check.get(..8).unwrap().to_vec();
            bytes.extend(rest);
            let pool = create_pool();
            let path = Path::new(FILE);
            node.files().remove(path).await.unwrap();
            let len = u64::try_from(bytes.len()).unwrap();
            let file = node.files().open(path, Mode::Create { len }).await;
            let file = file.unwrap();
            let block = crate::bytes::block(&pool, &bytes).unwrap();
            file.write_at(0, &[block]).await.unwrap();
            file.sync().await.unwrap();
        })
        .unwrap();
        let error = refused(&mut sim, &node, region);
        let path = PathBuf::from(FILE);
        assert_eq!(error, Error::Unfounded { path }, "version {version}");
    }
}

/// A power cut in a first open, before the log holds a record, leaves a first open,
/// also with another founding, and the next open keeps it.
#[test]
fn a_power_cut_in_the_first_open_leaves_a_first_open() {
    let (mut cut_before, mut cut_after) = (0_usize, 0_usize);
    for step in 0..64 {
        let mut sim = Sim::new(sim::Config {
            seed: step,
            ..sim::Config::default()
        });
        let node = sim.node(sim::node::Config::default());
        let region = sim
            .run_on(&node, |node, tasks| async move {
                create_region(&node, &tasks).await
            })
            .unwrap();
        let shard = env::shards::Config {
            name: "open".into(),
            core: None,
        };
        let (own, first) = (node.clone(), region.clone());
        let started = node.shards().start(shard, move |tasks| async move {
            let config = Config {
                founding: first,
                ..config(&own, &tasks, 1, &IDS, &IDS).await
            };
            let _mesh = Mesh::start(config).await;
            own.clock().sleep(TICK).await;
        });
        drop(started.unwrap());
        sim.run_for(Span::from_nanos(
            i64::try_from(step).unwrap().saturating_mul(100_000),
        ))
        .unwrap();
        sim.crash(&node, Crash::Power);
        let kept = sim
            .run_on(&node, |node, _| async move {
                node.files().list(Path::new("")).await.unwrap()
            })
            .unwrap();
        if kept.contains(&PathBuf::from(FILE)) {
            cut_after = cut_after.saturating_add(1);
        } else {
            cut_before = cut_before.saturating_add(1);
        }
        assert_eq!(
            logged(&mut sim, &node),
            log::Stored::default(),
            "step {step}"
        );
        let other = region::Founding {
            voters: [key(1)].into(),
            ..region.clone()
        };
        run_with(&mut sim, &node, other.clone());
        write_record(&mut sim, &node);
        let error = refused(&mut sim, &node, region.clone());
        assert_eq!(error, mismatch(&other, &region), "step {step}");
    }
    assert_ne!(cut_before, 0, "no cut came before the founding was kept");
    assert_ne!(cut_after, 0, "no cut came after the founding was kept");
}

#[test]
fn memory_that_the_system_refuses_for_the_founding_file_gives_the_pool_error() {
    solo(|node, tasks| async move {
        let budget = block::Config { budget: 4 << 20 };
        let (memory, switch) = Scarce::new(budget.reservation());
        let config = Config {
            pool: Rc::new(Pool::new(budget, memory)),
            ..config(&node, &tasks, 1, &IDS, &IDS).await
        };
        switch.refuse();
        let error = Mesh::start(config).await.err().unwrap();
        let cause = block::Error::Refused { requested: 831 };
        assert_eq!(error, Error::Pool(cause));
        assert_eq!(
            error.to_string(),
            "the pool has no block for the mesh now: the system refused memory for a \
             block of 831 bytes"
        );
    });
}

/// The log opens in blocks of 64 KiB, which the pool holds already, and the system
/// then refuses memory for the block of the founding read.
#[test]
fn memory_that_the_system_refuses_for_the_founding_read_gives_the_pool_error() {
    let (mut sim, node, region) = founded(0);
    let error = sim
        .run_on(&node, |node, tasks| async move {
            let budget = block::Config { budget: 4 << 20 };
            let (memory, switch) = Scarce::new(budget.reservation());
            let pool = Rc::new(Pool::new(budget, memory));
            drop(pool.alloc(crate::file::CHUNK).unwrap());
            let config = Config {
                founding: region,
                pool,
                ..config(&node, &tasks, 1, &IDS, &IDS).await
            };
            switch.refuse();
            Mesh::start(config).await.err().unwrap()
        })
        .unwrap();
    let cause = block::Error::Refused { requested: 831 };
    assert_eq!(error, Error::Pool(cause));
    assert_eq!(
        error.to_string(),
        "the pool has no block for the mesh now: the system refused memory for a \
         block of 831 bytes"
    );
}

/// A power cut right after a first open keeps its founding, so an open after the
/// next record checks it.
#[test]
fn the_founding_is_durable_when_it_is_kept() {
    for seed in 0..16 {
        let mut sim = Sim::new(sim::Config {
            seed,
            ..sim::Config::default()
        });
        let node = sim.node(sim::node::Config::default());
        let region = sim
            .run_on(&node, |node, tasks| async move {
                let config = config(&node, &tasks, 1, &IDS, &IDS).await;
                found(&config).await;
                config.founding
            })
            .unwrap();
        sim.crash(&node, Crash::Power);
        write_record(&mut sim, &node);
        let other = region::Founding {
            voters: [key(1)].into(),
            ..region.clone()
        };
        let error = refused(&mut sim, &node, other.clone());
        assert_eq!(error, mismatch(&region, &other), "seed {seed}");
    }
}
