use std::ffi::{CStr, CString};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use connector::browse::{
    self, Access, Cursor, Device, End, Entry, Facts, Found, Page, Query, Scope, Shape,
    Values,
};
use connector::kind::Error;
use env::clock::Clock;
use types::sample::{self, Scalar, Type};

use super::common::{config, rig, set_result};
use super::{RELAY, Relay, relayed, run, serve};
use crate::browse::{BYTES, Server as Browse};
use crate::ffi::Status;
use crate::node::ns0::{self, HAS_COMPONENT, ORGANIZES};
use crate::node::{self, Key, Name, Node};
use crate::server::{self, Server, Variable};
use crate::value::Value;

const BROWSE_RESPONSE: [u8; 4] = [0x01, 0x00, 0x12, 0x02];
const BROWSE_NEXT_RESPONSE: [u8; 4] = [0x01, 0x00, 0x18, 0x02];
const OBJECTS: Node = Node::ns0(ns0::OBJECTS);
const BASE_DATA_TYPE: Node = Node::ns0(ns0::BASE_DATA_TYPE);
const DOUBLE: Node = Node::ns0(ns0::DOUBLE);
const ROOT: Node = Node {
    namespace: 1,
    key: Key::Number(7000),
};

fn named(name: &str) -> Node {
    Node {
        namespace: 1,
        key: Key::Name(Name::new(name).expect("a name")),
    }
}

/// The `Browse` or `BrowseNext` response of `message`, else `None`.
fn response(message: &[u8]) -> Option<[u8; 4]> {
    let kind = message.get(24..28)?;
    let browse = kind == BROWSE_RESPONSE || kind == BROWSE_NEXT_RESPONSE;
    (message.starts_with(b"MSG") && browse).then(|| kind.try_into().expect("4 bytes"))
}

/// Gives the output of `body` with a browse of a session through a relay that gives
/// each server message to `rewrite`, after `setup`.
fn relayed_browse<T: Send + 'static>(
    setup: impl FnOnce(&Server) + Send + 'static,
    rewrite: impl FnMut(&mut Vec<u8>) + Send + 'static,
    body: impl AsyncFnOnce(&mut Browse<'_>, &Clock) -> T + Send + 'static,
) -> T {
    let (out, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            setup(peer.server);
            let url = format!("opc.tcp://{}:{RELAY}", node.addresses()[0]);
            relayed(&node, &Relay::default(), rewrite, async {
                let mut client = peer.connect(&url).await.expect("a connect");
                let out = body(&mut Browse::new(&mut client), &node.clock()).await;
                client.close().await;
                out
            })
            .await
        })
        .await
    });
    out
}

/// What a walk of `query` gives, as [`relayed_browse`] runs it.
fn walked(
    setup: impl FnOnce(&Server) + Send + 'static,
    rewrite: impl FnMut(&mut Vec<u8>) + Send + 'static,
    query: Query,
) -> Result<Found, Error> {
    relayed_browse(setup, rewrite, async move |server, clock| {
        browse::walk(server, &query, clock).await
    })
}

/// Puts `to` in place of each copy of `from` in `message`, and sets the size of the
/// message.
fn replace(message: &mut Vec<u8>, from: &[u8], to: &[u8]) {
    let mut at = 0;
    while at + from.len() <= message.len() {
        if &message[at..at + from.len()] == from {
            message.splice(at..at + from.len(), to.iter().copied());
            at += to.len();
        } else {
            at += 1;
        }
    }
    let size = u32::try_from(message.len()).expect("a message of a u32 of bytes");
    message[4..8].copy_from_slice(&size.to_le_bytes());
}

/// A setup with the folder `root` under the Objects folder, and a Double variable
/// under it of each node, name, and value of `variables`.
fn tree<N: AsRef<CStr> + Send + 'static>(
    variables: Vec<(Node, N, Option<f64>)>,
) -> impl FnOnce(&Server) + Send + 'static {
    move |server| {
        server.add_folder(&ROOT, &OBJECTS, ORGANIZES, c"root", c"Root");
        server.add_data_type(&DOUBLE, &BASE_DATA_TYPE, c"Double");
        for (node, name, value) in variables {
            server.add_variable(&Variable {
                node,
                parent: ROOT,
                reference: HAS_COMPONENT,
                name: name.as_ref(),
                data_type: DOUBLE,
                rank: -1,
                access: server::READ,
                value: value.map(|value| server::variant(Value::F64(value)).1),
            });
        }
    }
}

fn level() -> Query {
    Query {
        start: "root".parse().expect("a path"),
        scope: Scope::Level(None),
        values: Values::Read,
    }
}

fn entry(path: &str, tag: &str, shape: Shape) -> Entry {
    Entry {
        path: path.parse().expect("a path"),
        tag: tag.to_owned(),
        shape,
    }
}

fn double() -> Shape {
    Shape::Point(Facts {
        data_type: Some(Type::Scalar(Scalar::F64)),
        unit: None,
        unit_code: None,
        access: Some(Access::Read),
    })
}

/// The node `ns=1;s=x%<F0 9F>`, a name that is not UTF-8, and the node
/// `ns=1;s=x%<U+FFFD>`, which its lossy text names, are two nodes of one folder.
#[test]
fn a_name_that_is_not_utf8_gives_an_escaped_tag_and_hides_no_other_node() {
    let setup = tree(vec![
        (named("x%bc"), c"a", Some(1.5)),
        (named("x%\u{FFFD}"), c"b", Some(2.5)),
    ]);
    let rewrite = |message: &mut Vec<u8>| {
        if response(message).is_some() {
            let to = [4, 0, 0, 0, b'x', b'%', 0xF0, 0x9F];
            replace(message, &[4, 0, 0, 0, b'x', b'%', b'b', b'c'], &to);
        }
    };
    let found = walked(setup, rewrite, level()).expect("found");
    let reason = "its node ID is a name that is empty or not UTF-8, which the kind \
                  cannot read";
    assert_eq!(
        found.entries,
        [
            entry(
                "root/a",
                "ns=1;s%=x%25%F0%9F",
                Shape::Unreadable(reason.into())
            ),
            entry("root/b", "ns=1;s=x%\u{FFFD}", double()),
        ]
    );
    assert_eq!(found.values, [None, Some(sample::Value::F64(2.5))]);
}

/// The relay moves the node `ns=1;s=x%bc` to namespace 0 with a name that is not
/// UTF-8, whose tag has no `ns=` and is no node.
#[test]
fn a_name_of_namespace_0_that_is_not_utf8_gives_a_tag_that_is_no_node() {
    let setup = tree(vec![(named("x%bc"), c"a", Some(1.5))]);
    let rewrite = |message: &mut Vec<u8>| {
        if response(message).is_some() {
            let from = [3, 1, 0, 4, 0, 0, 0, b'x', b'%', b'b', b'c'];
            let to = [3, 0, 0, 4, 0, 0, 0, b'x', b'%', 0xF0, 0x9F];
            replace(message, &from, &to);
        }
    };
    let found = walked(setup, rewrite, level()).expect("found");
    let reason = "its node ID is a name that is empty or not UTF-8, which the kind \
                  cannot read";
    let tag = "s%=x%25%F0%9F";
    let unreadable = Shape::Unreadable(reason.into());
    assert_eq!(found.entries, [entry("root/a", tag, unreadable)]);
    assert_eq!(tag.parse::<Node>(), Err(node::Error::Syntax));
}

/// The server index 1 on the node `ns=1;s=xabc` makes it a node of another server.
#[test]
fn a_node_of_another_server_gives_its_text_as_its_tag() {
    let setup = tree(vec![(named("xabc"), c"a", Some(1.5))]);
    let rewrite = |message: &mut Vec<u8>| {
        if response(message).is_some() {
            let id = [1, 0, 4, 0, 0, 0, b'x', b'a', b'b', b'c'];
            let from = [&[3][..], &id].concat();
            let to = [&[0x43][..], &id, &[1, 0, 0, 0]].concat();
            replace(message, &from, &to);
        }
    };
    let found = walked(setup, rewrite, level()).expect("found");
    let reason = "it is a node of another server, which the kind cannot read";
    assert_eq!(
        found.entries,
        [entry(
            "root/a",
            "svr=1;ns=1;s=xabc",
            Shape::Unreadable(reason.into())
        )]
    );
    assert_eq!(found.values, [None]);
}

#[test]
fn a_variable_of_value_rank_0_is_unreadable_as_an_array() {
    let setup = |server: &Server| {
        tree::<&CStr>(Vec::new())(server);
        server.add_variable(&Variable {
            node: named("x"),
            parent: ROOT,
            reference: HAS_COMPONENT,
            name: c"a",
            data_type: DOUBLE,
            rank: 0,
            access: server::READ,
            value: None,
        });
    };
    let found = walked(setup, |_| {}, level()).expect("found");
    let array = Shape::Unreadable("an array".into());
    assert_eq!(found.entries, [entry("root/a", "ns=1;s=x", array)]);
}

/// Each page of `root` holds one reference. The cursor of each page goes on from the
/// continuation point that its browse holds.
#[test]
fn the_cursors_of_one_browse_take_one_browse_and_a_browse_next_for_each_page() {
    let setup = |server: &Server| {
        let names = [c"a", c"b", c"c"];
        let nodes = ["a", "b", "c"].map(named);
        tree(
            nodes
                .into_iter()
                .zip(names)
                .map(|(n, m)| (n, m, None))
                .collect(),
        )(server);
        server.cap_references(1);
    };
    let counts = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
    let counted = Arc::clone(&counts);
    let rewrite = move |message: &mut Vec<u8>| match response(message) {
        Some(BROWSE_RESPONSE) => _ = counted[0].fetch_add(1, Ordering::Relaxed),
        Some(_) => _ = counted[1].fetch_add(1, Ordering::Relaxed),
        None => {}
    };
    let pages = relayed_browse(setup, rewrite, async |server, _| {
        let root = entry("root", &ROOT.to_string(), Shape::Folder);
        let mut pages: Vec<Page> = Vec::new();
        let mut cursor = None;
        loop {
            let page = server.level(Some(&root), cursor.as_ref()).await;
            let page = page.expect("a page").expect("a folder");
            cursor = page.next.clone();
            pages.push(page);
            if cursor.is_none() {
                return pages;
            }
        }
    });
    let names: Vec<(Vec<String>, Option<Cursor>)> = pages
        .into_iter()
        .map(|page| {
            let names = page.entries.into_iter().map(|child| child.name).collect();
            (names, page.next)
        })
        .collect();
    let cursor = |text: &str| Some(Cursor::new(text.to_owned()));
    assert_eq!(
        names,
        [
            (vec!["a".to_owned()], cursor("1")),
            (vec!["b".to_owned()], cursor("2")),
            (vec!["c".to_owned()], None),
        ]
    );
    let counts = counts.each_ref().map(|count| count.load(Ordering::Relaxed));
    assert_eq!(counts, [1, 2], "Browse and BrowseNext responses");
}

/// The children of `root` in a page each, each rewritten to the class `ObjectType`,
/// which the client drops, so each page of `root` has no entry and a continuation
/// point. A walk counts a page with no entries as one visit and ends `Budget` past
/// `VISITS` (10 000).
#[test]
fn pages_with_no_entries_end_at_the_budget_of_a_walk() {
    const CHILDREN: u32 = 10_050;
    let setup = |server: &Server| {
        server.add_folder(&ROOT, &OBJECTS, ORGANIZES, c"root", c"Root");
        for index in 0..CHILDREN {
            let name = format!("child{index}\0");
            let name = CStr::from_bytes_with_nul(name.as_bytes()).expect("one NUL");
            let node = Node {
                namespace: 1,
                key: Key::Number(20_000 + index),
            };
            server.add_folder(&node, &ROOT, ORGANIZES, name, name);
        }
        server.cap_references(1);
    };
    let rewrite = move |message: &mut Vec<u8>| {
        if response(message).is_none() {
            return;
        }
        let mut found = Vec::new();
        let mut at = 0;
        while at + 9 <= message.len() {
            let length = i32::from_le_bytes(message[at..at + 4].try_into().expect("4"));
            let start = at + 4;
            if (6..=10).contains(&length) && message[start..].starts_with(b"child") {
                let end = start + usize::try_from(length).expect("positive");
                if end <= message.len()
                    && message[start + 5..end].iter().all(u8::is_ascii_digit)
                {
                    found.push(end);
                    at = end;
                    continue;
                }
            }
            at += 1;
        }
        for pair in found.chunks(2) {
            if let [_, display] = pair {
                let class = &mut message[*display..*display + 4];
                assert_eq!(class, &1_u32.to_le_bytes(), "the class of an Object");
                class.copy_from_slice(&8_u32.to_le_bytes());
            }
        }
    };
    let query = Query {
        start: "root".parse().expect("a path"),
        scope: Scope::Match("**".parse().expect("a pattern")),
        values: Values::Skipped,
    };
    let found = walked(setup, rewrite, query).expect("found");
    assert_eq!(found.end, End::Budget);
}

/// A setup with a tree that a walk of [`everything`] holds 4 points of at once, with
/// the point of the Objects folder when it also holds the variable of
/// `Server::report_points`: the folders `root/a/c`, `root/a/d`, `root/a/e`,
/// and `root/b`, two variables in each of `b`, `c`, `d`, and `e`, and one reference in
/// each page of the server. The folder `a` has a lower number than its parent `root`,
/// so the order of the keys of the points is not the order of their use.
fn nested(server: &Server) {
    let folder = |number| Node {
        namespace: 1,
        key: Key::Number(number),
    };
    server.add_folder(&ROOT, &OBJECTS, ORGANIZES, c"root", c"Root");
    server.add_data_type(&DOUBLE, &BASE_DATA_TYPE, c"Double");
    let folders = [
        (6001, ROOT, c"a"),
        (6002, ROOT, c"b"),
        (6003, folder(6001), c"c"),
        (6004, folder(6001), c"d"),
        (6005, folder(6001), c"e"),
    ];
    for (number, parent, name) in folders {
        server.add_folder(&folder(number), &parent, ORGANIZES, name, name);
    }
    for (number, parent) in [(7010, 6002), (7020, 6003), (7030, 6004), (7040, 6005)] {
        for (offset, name) in [c"x", c"y"].into_iter().enumerate() {
            let offset = u32::try_from(offset).expect("a u32");
            server.add_variable(&Variable {
                node: folder(number + offset),
                parent: folder(parent),
                reference: HAS_COMPONENT,
                name,
                data_type: DOUBLE,
                rank: -1,
                access: server::READ,
                value: None,
            });
        }
    }
    server.cap_references(1);
}

fn everything() -> Query {
    Query {
        start: "root".parse().expect("a path"),
        scope: Scope::Match("**".parse().expect("a pattern")),
        values: Values::Skipped,
    }
}

/// What a walk of `query` gives after `setup` when the server gives `most` as its
/// `MaxBrowseContinuationPoints`, as [`counted`] gives it.
fn held(
    setup: fn(&Server),
    query: Query,
    most: u16,
) -> (Result<Found, Error>, usize, usize) {
    counted(
        move |server| {
            setup(server);
            server.report_points(most);
        },
        query,
    )
}

/// What a walk of `query` gives after `setup`, the most points that the session held,
/// and the count of Browse requests.
fn counted(
    setup: impl FnOnce(&Server) + Send + 'static,
    query: Query,
) -> (Result<Found, Error>, usize, usize) {
    let (out, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            setup(peer.server);
            let count = peer.server.count();
            let mut client = peer.connected().await;
            let found =
                browse::walk(&mut Browse::new(&mut client), &query, &node.clock())
                    .await;
            client.close().await;
            (found, count.points(), count.browses())
        })
        .await
    });
    out
}

/// With no bound from the server, a walk holds a point for each open folder: the
/// Objects folder, `root`, `root/a`, and `root/a/c`. With a bound of 1 or 2, it holds
/// no more, and gives the same entries. At the bound of 2, it releases the point of
/// `root`, used least lately, so it browses `root` once more and `root/a` no more.
#[test]
fn a_walk_holds_no_more_points_than_the_server_holds_for_a_session() {
    let (unbound, points, browses) = held(nested, everything(), 0);
    let unbound = unbound.expect("found");
    assert_eq!(unbound.entries.len(), 8, "{:?}", unbound.entries);
    assert_eq!((points, browses), (4, 7));
    for (most, count) in [(1, 10), (2, 8)] {
        let (found, points, browses) = held(nested, everything(), most);
        assert_eq!(found.expect("found").entries, unbound.entries);
        assert_eq!(
            (points, browses),
            (usize::from(most), count),
            "bound {most}"
        );
    }
}

/// A walk of paths makes one Browse request for each of Objects, `root`, `a`, `b`,
/// `c`, and `e`, and none for `d`, which no path is under. It stops `b` and `c` when it
/// has seen `x`, so it holds the points of Objects, `b`, and `c`.
#[test]
fn a_walk_of_paths_gives_the_entry_at_each_in_one_browse_of_each_folder() {
    let paths =
        ["root/a/e/y", "root/b/x", "root/a/c/x"].map(|p| p.parse().expect("a path"));
    let query = Query {
        start: "root".parse().expect("a path"),
        scope: Scope::Points(paths.to_vec()),
        values: Values::Skipped,
    };
    let (found, points, browses) = held(nested, query, 0);
    let found = found.expect("found");
    let paths: Vec<String> = found.entries.iter().map(|e| e.path.to_string()).collect();
    assert_eq!(paths, ["root/a/e/y", "root/b/x", "root/a/c/x"]);
    assert_eq!(found.end, End::Done);
    assert_eq!((points, browses), (3, 6));
}

/// Gives each server message to `rewrite`, and makes the result of the Browse
/// response of the count `nth` from 1 `BadNoContinuationPoints`.
fn refusing(nth: usize) -> impl FnMut(&mut Vec<u8>) + Send + 'static {
    let mut browses = 0;
    move |message: &mut Vec<u8>| {
        if response(message) != Some(BROWSE_RESPONSE) {
            return;
        }
        browses += 1;
        if browses == nth {
            // The response header of open62541 is 24 bytes, then the count of results.
            assert_eq!(message[52..56], 1_i32.to_le_bytes(), "one result");
            assert_eq!(message[56..60], [0; 4], "a Good result");
            let status = Status::BAD_NO_CONTINUATION_POINTS.0.to_le_bytes();
            message[56..60].copy_from_slice(&status);
        }
    }
}

/// The server refuses a point for the Browse of `root/a/c`, while the walk holds the
/// points of Objects, `root`, and `root/a`: the walk releases each, browses `c` again,
/// and gives each entry. It then browses `root` and `a` again for their next pages, 10
/// Browse requests in all, against 7 with no refusal.
#[test]
fn a_browse_that_the_server_gives_no_point_releases_its_points_and_browses_again() {
    let (unbound, ..) = held(nested, everything(), 0);
    let browses = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&browses);
    let mut refuse = refusing(4);
    let rewrite = move |message: &mut Vec<u8>| {
        if response(message) == Some(BROWSE_RESPONSE) {
            counted.fetch_add(1, Ordering::Relaxed);
        }
        refuse(message);
    };
    let setup = |server: &Server| {
        nested(server);
        server.report_points(0);
    };
    let found = relayed_browse(setup, rewrite, async |server, clock| {
        browse::walk(server, &everything(), clock).await
    });
    assert_eq!(
        found.expect("found").entries,
        unbound.expect("found").entries
    );
    assert_eq!(browses.load(Ordering::Relaxed), 10);
}

/// The server refuses a point for the first Browse, of the Objects folder, when the
/// walk holds none to release: the walk browses once again.
#[test]
fn a_browse_that_the_server_gives_no_point_with_none_held_browses_again() {
    let (unbound, ..) = held(nested, everything(), 0);
    let found = relayed_browse(nested, refusing(1), async |server, clock| {
        browse::walk(server, &everything(), clock).await
    });
    assert_eq!(
        found.expect("found").entries,
        unbound.expect("found").entries
    );
}

/// The server refuses a point for the Browse of `root/a/c`, while the walk holds the
/// points of Objects, `root`, and `root/a`, and again after the walk releases them.
#[test]
fn a_second_refusal_after_a_release_fails() {
    let mut first = refusing(4);
    let mut second = refusing(5);
    let setup = |server: &Server| {
        nested(server);
        server.report_points(0);
    };
    let rewrite = move |message: &mut Vec<u8>| {
        first(message);
        second(message);
    };
    let found = relayed_browse(setup, rewrite, async |server, clock| {
        browse::walk(server, &everything(), clock).await
    });
    let Err(Error::Device(source)) = found else {
        panic!("not a device error: {found:?}");
    };
    assert_eq!(
        source.to_string(),
        "the browse of opc.tcp://10.0.0.1:4841 failed with BadNoContinuationPoints"
    );
}

/// A setup with the loop `root/f/g/f`, a variable in `f` and in `g`, and one reference
/// in each page of the server.
fn looped(server: &Server) {
    let folder = |number| Node {
        namespace: 1,
        key: Key::Number(number),
    };
    server.add_folder(&ROOT, &OBJECTS, ORGANIZES, c"root", c"Root");
    server.add_data_type(&DOUBLE, &BASE_DATA_TYPE, c"Double");
    server.add_folder(&folder(6001), &ROOT, ORGANIZES, c"f", c"f");
    server.add_folder(&folder(6002), &folder(6001), ORGANIZES, c"g", c"g");
    for (number, parent, name) in [(7001, 6001, c"x"), (7002, 6002, c"w")] {
        server.add_variable(&Variable {
            node: folder(number),
            parent: folder(parent),
            reference: HAS_COMPONENT,
            name,
            data_type: DOUBLE,
            rank: -1,
            access: server::READ,
            value: None,
        });
    }
    server.add_reference(&folder(6002), ORGANIZES, &folder(6001));
    server.cap_references(1);
}

/// A walk of a pattern of fixed depth through a loop opens `f` and `g` more than once,
/// so a page gives a cursor that the walk holds. It releases the earlier point, so it
/// holds no more points than the bound.
#[test]
fn a_folder_open_twice_in_a_walk_holds_no_more_points_than_the_bound() {
    for pattern in ["*/*/*/*/*", "*/*/*/*/*/*/*"] {
        let query = |pattern: &str| Query {
            start: "root".parse().expect("a path"),
            scope: Scope::Match(pattern.parse().expect("a pattern")),
            values: Values::Skipped,
        };
        let (unbound, ..) = held(looped, query(pattern), 0);
        let unbound = unbound.expect("found").entries;
        for most in 2..=4 {
            let (found, points, _) = held(looped, query(pattern), most);
            assert_eq!(found.expect("found").entries, unbound, "{pattern} {most}");
            assert!(points <= usize::from(most), "{pattern} {most}: {points}");
        }
    }
}

/// A setup with the chain `root/f1/f2/f3/f4/f5`, a variable in `root` and in each of
/// `f1` to `f4`, and one reference in each page of the server, so that a walk holds a
/// point for the Objects folder, `root`, and `f1` to `f4` at once.
fn chain(server: &Server) {
    let folder = |number| Node {
        namespace: 1,
        key: Key::Number(number),
    };
    server.add_folder(&ROOT, &OBJECTS, ORGANIZES, c"root", c"Root");
    server.add_data_type(&DOUBLE, &BASE_DATA_TYPE, c"Double");
    let mut parent = ROOT;
    for (number, name) in (6001..).zip([c"f1", c"f2", c"f3", c"f4", c"f5"]) {
        server.add_folder(&folder(number), &parent, ORGANIZES, name, name);
        server.add_variable(&Variable {
            node: folder(number + 1000),
            parent: parent.clone(),
            reference: HAS_COMPONENT,
            name: c"x",
            data_type: DOUBLE,
            rank: -1,
            access: server::READ,
            value: None,
        });
        parent = folder(number);
    }
    server.cap_references(1);
}

/// A server that gives 0 as its bound bounds a walk at `DEPTH` (64), so a walk holds
/// the points of the Objects folder, `root`, and `f1` to `f4` at once.
#[test]
fn a_walk_with_a_bound_of_0_holds_a_point_for_each_open_folder() {
    let (found, points, _) = held(chain, everything(), 0);
    assert_eq!(found.expect("found").entries.len(), 5);
    assert_eq!(points, 6);
}

/// A server with no `MaxBrowseContinuationPoints`, or with a `UInt32` of 1 as it,
/// bounds a walk at `DEPTH`, as one that gives 0 does. The variable `ns=1;i=2735` is
/// not the bound: it gives the Objects folder a second reference, as the other does.
#[test]
fn a_walk_with_no_bound_of_the_server_holds_a_point_for_each_open_folder() {
    let absent = |server: &Server| {
        chain(server);
        let node = Node {
            namespace: 1,
            key: Key::Number(2735),
        };
        server.add_scalar(&node, Value::U32(1));
    };
    let wide = |server: &Server| {
        chain(server);
        server.add_scalar(&Node::ns0(2735), Value::U32(1));
    };
    let setups: [fn(&Server); 2] = [absent, wide];
    for (index, setup) in setups.into_iter().enumerate() {
        let (found, points, _) = counted(setup, everything());
        assert_eq!(found.expect("found").entries.len(), 5, "setup {index}");
        assert_eq!(points, 6, "setup {index}");
    }
}

/// A walk whose Browse and `BrowseNext` answers have a service result that is Good
/// with a subcode or Uncertain finds what a walk of Good answers finds.
#[test]
fn a_walk_whose_answers_are_not_bad_finds_their_entries() {
    let good = walked(nested, |_: &mut Vec<u8>| {}, everything()).expect("found");
    for status in super::NOT_GOOD {
        let rewrite = move |message: &mut Vec<u8>| {
            if response(message).is_some() {
                set_result(message, status);
            }
        };
        let found = walked(nested, rewrite, everything());
        assert_eq!(found.expect("found"), good, "{status:?}");
    }
}

/// Gives each server message to `rewrite`, and makes the service result of the first
/// message whose response is `kind` and that `pick` accepts `BadInternalError`.
fn faulting(
    kind: [u8; 4],
    mut pick: impl FnMut(&[u8]) -> bool + Send + 'static,
) -> impl FnMut(&mut Vec<u8>) + Send + 'static {
    let mut done = false;
    move |message: &mut Vec<u8>| {
        let response = message.get(24..28).is_some_and(|got| got == kind);
        if !done && message.starts_with(b"MSG") && response && pick(message) {
            done = true;
            set_result(message, Status::BAD_INTERNAL_ERROR);
        }
    }
}

fn internal_error(found: Result<Found, Error>) {
    let Err(Error::Device(source)) = found else {
        panic!("not a device error: {found:?}");
    };
    assert_eq!(
        source.to_string(),
        "the browse of opc.tcp://10.0.0.1:4841 failed with BadInternalError"
    );
}

/// The first Read after the connect is of the bound of the server and the limits of a
/// Browse and a Translate. It fails, so the walk fails.
#[test]
fn a_walk_whose_read_of_the_bound_fails_fails() {
    const READ_RESPONSE: [u8; 4] = [0x01, 0x00, 0x7A, 0x02];
    let connected = Arc::new(AtomicBool::new(false));
    let armed = Arc::clone(&connected);
    let rewrite = faulting(READ_RESPONSE, move |_| armed.load(Ordering::Relaxed));
    let found = relayed_browse(nested, rewrite, async move |server, clock| {
        connected.store(true, Ordering::Relaxed);
        browse::walk(server, &everything(), clock).await
    });
    internal_error(found);
}

/// Whether the `BrowseNext` response `message` is that of a release: one result with
/// no reference, which no page of a tree with one reference in each page gives.
fn released(message: &[u8]) -> bool {
    // The results start at 52: their count, then the status, the point, and the count
    // of the references of the first.
    let point = i32::from_le_bytes(message[60..64].try_into().expect("4 bytes"));
    let at = 64 + usize::try_from(point.max(0)).expect("a size");
    message[52..56] == 1_i32.to_le_bytes()
        && i32::from_le_bytes(message[at..at + 4].try_into().expect("4 bytes")) <= 0
}

/// The `BrowseNext` that releases the first point at the bound of 1 fails, so the walk
/// fails.
#[test]
fn a_walk_whose_release_fails_fails() {
    let setup = |server: &Server| {
        nested(server);
        server.report_points(1);
    };
    let found = relayed_browse(
        setup,
        faulting(BROWSE_NEXT_RESPONSE, released),
        async |server, clock| browse::walk(server, &everything(), clock).await,
    );
    internal_error(found);
}

/// With no bound release, the first release of a walk of `*/*/*/*/*` through a loop is
/// of the earlier point of a folder that it opens twice. It fails, so the walk fails.
#[test]
fn a_walk_whose_release_of_a_point_of_a_folder_open_twice_fails_fails() {
    let setup = |server: &Server| {
        looped(server);
        server.report_points(0);
    };
    let query = Query {
        start: "root".parse().expect("a path"),
        scope: Scope::Match("*/*/*/*/*".parse().expect("a pattern")),
        values: Values::Skipped,
    };
    let found = relayed_browse(
        setup,
        faulting(BROWSE_NEXT_RESPONSE, released),
        async move |server, clock| browse::walk(server, &query, clock).await,
    );
    internal_error(found);
}

/// The release after the refusal of the Browse of `root/a/c`, the first `BrowseNext`
/// after it, fails, so the walk fails.
#[test]
fn a_walk_whose_release_after_a_refusal_fails_fails() {
    let setup = |server: &Server| {
        nested(server);
        server.report_points(0);
    };
    let mut browses = 0;
    let mut refuse = refusing(4);
    let mut fault = faulting(BROWSE_NEXT_RESPONSE, |_| true);
    let rewrite = move |message: &mut Vec<u8>| {
        browses += usize::from(response(message) == Some(BROWSE_RESPONSE));
        refuse(message);
        if browses >= 4 {
            fault(message);
        }
    };
    let found = relayed_browse(setup, rewrite, async |server, clock| {
        browse::walk(server, &everything(), clock).await
    });
    internal_error(found);
}

/// The references of `root` in a page each, the second rewritten to the class
/// `ObjectType`, which the client drops, so the second page of the server has no
/// references. A new browse of each cursor, as a `Scope::Level` browse of the kind
/// does, reaches the third reference.
#[test]
fn a_new_browse_of_each_cursor_goes_past_a_server_page_with_no_references() {
    let setup = |server: &Server| {
        server.add_folder(&ROOT, &OBJECTS, ORGANIZES, c"root", c"Root");
        for (index, name) in [c"keep0", c"drop1", c"keep2"].into_iter().enumerate() {
            let node = Node {
                namespace: 1,
                key: Key::Number(20_000 + u32::try_from(index).expect("small")),
            };
            server.add_folder(&node, &ROOT, ORGANIZES, name, name);
        }
        server.cap_references(1);
    };
    let rewrite = |message: &mut Vec<u8>| {
        if response(message).is_none() {
            return;
        }
        let pattern = [&5_i32.to_le_bytes()[..], b"drop1"].concat();
        let ends: Vec<usize> = message
            .windows(pattern.len())
            .enumerate()
            .filter(|(_, window)| *window == pattern.as_slice())
            .map(|(at, _)| at + pattern.len())
            .collect();
        for pair in ends.chunks(2) {
            if let [_, display] = pair {
                let class = &mut message[*display..*display + 4];
                assert_eq!(class, &1_u32.to_le_bytes(), "the class of an Object");
                class.copy_from_slice(&8_u32.to_le_bytes());
            }
        }
    };
    let (pages, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            setup(peer.server);
            let url = format!("opc.tcp://{}:{RELAY}", node.addresses()[0]);
            relayed(&node, &Relay::default(), rewrite, async {
                let mut client = peer.connect(&url).await.expect("a connect");
                let root = entry("root", &ROOT.to_string(), Shape::Folder);
                let mut pages: Vec<(Vec<String>, Option<Cursor>)> = Vec::new();
                let mut cursor: Option<Cursor> = None;
                for _ in 0..6 {
                    let mut browse = Browse::new(&mut client);
                    let page = browse.level(Some(&root), cursor.as_ref()).await;
                    let page = page.expect("a page").expect("a folder");
                    let names = page.entries.into_iter().map(|child| child.name);
                    cursor = page.next.clone();
                    pages.push((names.collect(), page.next));
                    if cursor.is_none() {
                        break;
                    }
                }
                client.close().await;
                pages
            })
            .await
        })
        .await
    });
    let names: Vec<String> = pages.iter().flat_map(|(n, _)| n.clone()).collect();
    assert_eq!(names, ["keep0", "keep2"], "pages: {pages:?}");
    let last = pages.last().map(|(_, next)| next.clone());
    assert_eq!(last, Some(None), "pages: {pages:?}");
}

/// `count` folders `n00000` and on under `root`, in a page each.
fn numbered(count: u32) -> impl FnOnce(&Server) + Send + 'static {
    move |server: &Server| {
        server.add_folder(&ROOT, &OBJECTS, ORGANIZES, c"root", c"Root");
        for index in 0..count {
            let node = Node {
                namespace: 1,
                key: Key::Number(20_000 + index),
            };
            let name = CString::new(format!("n{index:05}")).expect("no NUL");
            server.add_folder(&node, &ROOT, ORGANIZES, &name, &name);
        }
        server.cap_references(1);
    }
}

/// Gives each Browse and `BrowseNext` response to `change` with the end of the display
/// name of each of its references of [`numbered`], and the place of that reference in
/// the order of the server.
fn placed(
    mut change: impl FnMut(&mut Vec<u8>, usize, usize) + Send + 'static,
) -> impl FnMut(&mut Vec<u8>) + Send + 'static {
    let mut order = std::collections::BTreeMap::new();
    move |message: &mut Vec<u8>| {
        if response(message).is_none() {
            return;
        }
        // Each name is the length 6, `n`, and 5 digits, as the browse name and then the
        // display name of its reference, which the class follows.
        let ends: Vec<(usize, usize)> = message
            .windows(10)
            .enumerate()
            .filter(|(_, window)| {
                window[..5] == [6, 0, 0, 0, b'n']
                    && window[5..].iter().all(u8::is_ascii_digit)
            })
            .map(|(at, window)| {
                let digits = std::str::from_utf8(&window[5..]).expect("digits");
                (at + 10, digits.parse().expect("a number"))
            })
            .collect();
        for pair in ends.chunks(2).rev() {
            if let [_, (display, index)] = pair {
                let next = order.len();
                let place = *order.entry(*index).or_insert(next);
                change(message, *display, place);
            }
        }
    }
}

/// What a new browse of `root` gives for each cursor from `cursor`, at most `pages`
/// times, through a relay that gives each server message to `rewrite`.
fn new_browses(
    setup: impl FnOnce(&Server) + Send + 'static,
    rewrite: impl FnMut(&mut Vec<u8>) + Send + 'static,
    cursor: Option<Cursor>,
    pages: usize,
) -> Result<Vec<(usize, Option<Cursor>)>, Error> {
    let (out, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            setup(peer.server);
            let url = format!("opc.tcp://{}:{RELAY}", node.addresses()[0]);
            relayed(&node, &Relay::default(), rewrite, async {
                let mut client = peer.connect(&url).await.expect("a connect");
                let root = entry("root", &ROOT.to_string(), Shape::Folder);
                let (mut out, mut cursor) = (Vec::new(), cursor);
                for _ in 0..pages {
                    let mut browse = Browse::new(&mut client);
                    let page = browse.level(Some(&root), cursor.as_ref()).await;
                    let page = match page {
                        Ok(page) => page.expect("a folder"),
                        Err(error) => {
                            client.close().await;
                            return Err(error);
                        }
                    };
                    cursor = page.next.clone();
                    out.push((page.entries.len(), page.next));
                    if cursor.is_none() {
                        break;
                    }
                }
                client.close().await;
                Ok(out)
            })
            .await
        })
        .await
    });
    out
}

/// 10 003 references under `root`, of which the server gives the second to the
/// 10 002nd, `VISITS` + 1 of them, as `ObjectType`, which the browse gives no entry
/// for. A new browse of each cursor counts them, so it goes past them.
#[test]
fn a_new_browse_of_each_cursor_goes_past_more_than_visits_references_of_other_classes()
{
    let rewrite = placed(|message, display, place| {
        if (1..=10_001).contains(&place) {
            let class = &mut message[display..display + 4];
            assert_eq!(class, &1_u32.to_le_bytes(), "the class of an Object");
            class.copy_from_slice(&8_u32.to_le_bytes());
        }
    });
    let pages = new_browses(numbered(10_003), rewrite, None, 4);
    let pages = pages.expect("the pages");
    assert_eq!(
        pages,
        [
            (1, Some(Cursor::new("1".to_owned()))),
            (0, Some(Cursor::new("2".to_owned()))),
            (0, Some(Cursor::new("3".to_owned()))),
            (0, Some(Cursor::new("4".to_owned()))),
        ]
    );
}

/// Two references named `same` under `root`, in a page each, of which the server gives
/// the first as `ObjectType`. A new browse of the cursor `1` gives the second its plain
/// name, as the browse that gave the cursor does.
#[test]
fn a_reference_of_another_class_before_a_cursor_takes_no_name() {
    let setup = |server: &Server| {
        server.add_folder(&ROOT, &OBJECTS, ORGANIZES, c"root", c"Root");
        add_folders(
            server,
            &ROOT,
            vec![(20_000, "same".to_owned()), (20_001, "same".to_owned())],
        );
        server.cap_references(1);
    };
    let mut first = true;
    let rewrite = move |message: &mut Vec<u8>| {
        let pattern = [&4_i32.to_le_bytes()[..], b"same"].concat();
        let at = message
            .windows(8)
            .position(|window| window == pattern.as_slice());
        if response(message).is_none() || !first {
            return;
        }
        // The display name `d` of `add_folders`, with an empty locale, then the class.
        let class = at.expect("a reference") + 8 + 10;
        assert_eq!(message[class..class + 4], 1_u32.to_le_bytes(), "an Object");
        message[class..class + 4].copy_from_slice(&8_u32.to_le_bytes());
        first = false;
    };
    let names = relayed_browse(setup, rewrite, async |server, _| {
        let root = entry("root", &ROOT.to_string(), Shape::Folder);
        let cursor = Cursor::new("1".to_owned());
        let page = server.level(Some(&root), Some(&cursor)).await;
        let page = page.expect("a page").expect("a folder");
        page.entries
            .into_iter()
            .map(|child| child.name)
            .collect::<Vec<_>>()
    });
    assert_eq!(names, ["same"]);
}

/// Makes the page of `message`, of one reference, a page with no references.
fn emptied(message: &mut Vec<u8>) {
    // The results start at 52: their count, then the status, the point, and the count
    // of the references of the first. The count of their diagnostics ends the message.
    assert_eq!(message[52..56], 1_i32.to_le_bytes(), "one result");
    let point = i32::from_le_bytes(message[60..64].try_into().expect("4 bytes"));
    let at = 64 + usize::try_from(point.max(0)).expect("a size");
    assert_eq!(message[at..at + 4], 1_i32.to_le_bytes(), "one reference");
    message[at..at + 4].copy_from_slice(&0_i32.to_le_bytes());
    let end = message.len() - 4;
    message.drain(at + 4..end);
    let size = u32::try_from(message.len()).expect("a size");
    message[4..8].copy_from_slice(&size.to_le_bytes());
}

/// What a new browse of the cursor `cursor` gives over 10 003 references in a page
/// each, of which the relay empties the pages of the second to the one at `last` in the
/// order of the server.
fn past_empty_pages(
    cursor: &str,
    last: usize,
) -> Result<Vec<(usize, Option<Cursor>)>, Error> {
    let rewrite = placed(move |message, _, place| {
        if (1..=last).contains(&place) {
            emptied(message);
        }
    });
    let cursor = Some(Cursor::new(cursor.to_owned()));
    new_browses(numbered(10_003), rewrite, cursor, 1)
}

/// The cursor `2` skips one reference, reads past `VISITS` (10 000) pages with no
/// references, skips one more, and gives the last.
#[test]
fn a_new_browse_of_a_cursor_reads_past_visits_pages_with_no_references() {
    let pages = past_empty_pages("2", 10_000).expect("the pages");
    assert_eq!(pages, [(1, None)]);
}

/// The cursor `1` skips one reference, and the next `VISITS` + 1 pages have no
/// references, so its page would have the cursor `1` again.
#[test]
fn a_new_browse_of_a_cursor_past_more_than_visits_pages_with_no_references_fails() {
    let pages = past_empty_pages("1", 10_001);
    let Err(Error::Device(source)) = pages else {
        panic!("not a device error: {pages:?}");
    };
    assert_eq!(
        source.to_string(),
        "the browse reads past 10000 pages with no references"
    );
}

/// Adds a folder under `parent` for each number and browse name of `children`, with
/// the display name `d`.
fn add_folders(server: &Server, parent: &Node, children: Vec<(u32, String)>) {
    for (number, name) in children {
        let node = Node {
            namespace: 1,
            key: Key::Number(number),
        };
        let name = CString::new(name).expect("no NUL");
        server.add_folder(&node, parent, ORGANIZES, &name, c"d");
    }
}

/// The folder `a` of [`sized`].
const A: Node = Node {
    namespace: 1,
    key: Key::Number(7001),
};

/// The folder `b` of [`sized`].
const B: Node = Node {
    namespace: 1,
    key: Key::Number(7002),
};

/// The folders `a` and `b` under `root`, 16 references in each page of the server,
/// and children of 4096 bytes of tag and name each: 32 in `a`, and 4080 in `b` with
/// `extra` more bytes in the name of the first.
fn sized(extra: usize) -> impl FnOnce(&Server) + Send + 'static {
    // The tag of each child, such as `ns=1;i=20000`, has 12 bytes.
    let name =
        |number: u32, extra: usize| format!("{}{number:0>4084}", "0".repeat(extra));
    move |server: &Server| {
        server.add_folder(&ROOT, &OBJECTS, ORGANIZES, c"root", c"Root");
        server.add_folder(&A, &ROOT, ORGANIZES, c"a", c"a");
        server.add_folder(&B, &ROOT, ORGANIZES, c"b", c"b");
        add_folders(
            server,
            &A,
            (20_000..20_032).map(|n| (n, name(n, 0))).collect(),
        );
        let b =
            (30_000..34_080).map(|n| (n, name(n, if n == 30_000 { extra } else { 0 })));
        add_folders(server, &B, b.collect());
        server.cap_references(16);
    }
}

/// The browse of [`sized`], whose server gives `most` as its
/// `MaxBrowseContinuationPoints`, reads the first page of `a` `opens` times and holds
/// its point, so it keeps 64 KiB for it, and the pages of `b` keep
/// `BYTES - 64 KiB + extra` at its last page.
fn budget(extra: usize, most: u16, opens: usize) -> Result<usize, Error> {
    let sized = sized(extra);
    let setup = move |server: &Server| {
        sized(server);
        server.report_points(most);
    };
    relayed_browse(
        setup,
        |_| {},
        async move |server, _| {
            let a = entry("root/a", &A.to_string(), Shape::Folder);
            for _ in 0..opens {
                let first = server.level(Some(&a), None).await?.expect("a folder");
                assert_eq!(first.next, Some(Cursor::new("16".to_owned())));
            }
            let b = entry("root/b", &B.to_string(), Shape::Folder);
            let (mut entries, mut cursor) = (0, None);
            loop {
                let page = server.level(Some(&b), cursor.as_ref()).await?;
                let page = page.expect("a folder");
                entries += page.entries.len();
                cursor = page.next;
                if cursor.is_none() {
                    return Ok(entries);
                }
            }
        },
    )
}

#[test]
fn a_browse_keeps_the_tags_and_names_of_its_budget() {
    assert_eq!(BYTES, 16 * 4096 + 4080 * 4096);
    assert_eq!(budget(0, 0, 1).expect("the pages of `b`"), 4080);
}

/// Past its budget, the browse releases the point of `a`, and with it 64 KiB.
#[test]
fn a_browse_past_its_budget_releases_a_held_point() {
    assert_eq!(budget(1, 0, 1).expect("the pages of `b`"), 4080);
}

/// The pages of `b` alone keep one byte past the budget.
#[test]
fn a_browse_that_keeps_more_than_its_budget_fails() {
    let found = budget(64 * 1024 + 1, 0, 1);
    let Err(Error::Device(source)) = found else {
        panic!("not a device error: {found:?}");
    };
    assert_eq!(
        source.to_string(),
        "the tags and names of the browse pass 16 MiB"
    );
}

/// The pages of `b` alone keep the budget to the byte, once the point of `a` is
/// released.
#[test]
fn a_browse_that_keeps_its_budget_to_the_byte_passes() {
    assert_eq!(budget(64 * 1024, 0, 1).expect("the pages of `b`"), 4080);
}

/// At the bound of 1, the first Browse of `b` releases the point of `a`, and with it
/// the 64 KiB that the browse kept for that point.
#[test]
fn a_browse_frees_the_budget_of_each_point_that_it_releases() {
    assert_eq!(budget(1, 1, 1).expect("the pages of `b`"), 4080);
}

/// The second page of `a` from its start gives the cursor of the first, so the browse
/// releases the earlier point and keeps the 64 KiB of `a` once. So past its budget,
/// the release of the point of `a` frees all that the browse keeps for `a`.
#[test]
fn a_folder_open_twice_keeps_its_budget_once() {
    assert_eq!(budget(1, 0, 2).expect("the pages of `b`"), 4080);
}

/// The browse holds the point of the first page of `a` (64 KiB) and of the pages of
/// `b` to 32 children before its end (`BYTES - 128 KiB + 1`), then reads the first page
/// of `a` again. That page gives the cursor that the browse holds, so the browse keeps
/// `BYTES - 64 KiB + 1` after it, as before it.
#[test]
fn a_folder_open_again_near_the_budget_keeps_its_budget() {
    let found = relayed_browse(
        sized(1),
        |_| {},
        async move |server, _| {
            let a = entry("root/a", &A.to_string(), Shape::Folder);
            let first = server.level(Some(&a), None).await?.expect("a folder");
            assert_eq!(first.next, Some(Cursor::new("16".to_owned())));
            let b = entry("root/b", &B.to_string(), Shape::Folder);
            let mut cursor = None;
            while cursor != Some(Cursor::new("4048".to_owned())) {
                let page = server.level(Some(&b), cursor.as_ref()).await?;
                cursor = page.expect("a folder").next;
            }
            let page = server.level(Some(&b), cursor.as_ref()).await?;
            assert_eq!(
                page.expect("a folder").next,
                Some(Cursor::new("4064".to_owned()))
            );
            server
                .level(Some(&a), None)
                .await
                .map(|page| page.expect("a folder").next)
        },
    );
    assert_eq!(found.expect("a page"), Some(Cursor::new("16".to_owned())));
}

/// The relay frees the point of the first cursor past `SKIP` (100 000) when the browse
/// goes on from it, and that browse fails, and does not read the folder again from its
/// start.
#[test]
fn a_cursor_past_the_skip_bound_whose_point_the_server_freed_fails() {
    let setup = |server: &Server| {
        server.add_folder(&ROOT, &OBJECTS, ORGANIZES, c"root", c"Root");
        let children = (0..100_600).map(|index| (200_000 + index, format!("c{index}")));
        add_folders(server, &ROOT, children.collect());
    };
    let freed = Arc::new(AtomicBool::new(false));
    let free = Arc::clone(&freed);
    let rewrite = move |message: &mut Vec<u8>| {
        if response(message) == Some(BROWSE_NEXT_RESPONSE)
            && free.swap(false, Ordering::Relaxed)
        {
            let invalid = Status::BAD_CONTINUATION_POINT_INVALID.0.to_le_bytes();
            message[56..60].copy_from_slice(&invalid);
        }
    };
    let page = relayed_browse(setup, rewrite, async move |server, _| {
        let root = entry("root", &ROOT.to_string(), Shape::Folder);
        let mut cursor: Option<Cursor> = None;
        loop {
            let page = server.level(Some(&root), cursor.as_ref()).await;
            let page = page.expect("a page").expect("a folder");
            cursor = page.next;
            let count: usize = cursor
                .as_ref()
                .expect("a cursor")
                .as_str()
                .parse()
                .expect("a count");
            if count > 100_000 {
                freed.store(true, Ordering::Relaxed);
                let page = server.level(Some(&root), cursor.as_ref()).await;
                return page;
            }
        }
    });
    let Err(Error::Device(source)) = page else {
        panic!("not a device error: {page:?}");
    };
    assert_eq!(
        source.to_string(),
        "the browse of opc.tcp://10.0.0.1:4841 failed with BadContinuationPointInvalid"
    );
}

/// What a walk of everything gives at the bound of `most` over 12 003 folders under
/// `root` in a page each, each with a variable, where the relay empties the page of
/// each place for which `empty` is true.
fn with_variables(
    most: u16,
    empty: impl Fn(usize) -> bool + Send + 'static,
) -> Result<Found, Error> {
    let numbered = numbered(12_003);
    let setup = move |server: &Server| {
        numbered(server);
        server.add_data_type(&DOUBLE, &BASE_DATA_TYPE, c"Double");
        for (number, parent) in (40_000..52_003).zip(20_000..32_003) {
            server.add_variable(&Variable {
                node: Node {
                    namespace: 1,
                    key: Key::Number(number),
                },
                parent: Node {
                    namespace: 1,
                    key: Key::Number(parent),
                },
                reference: HAS_COMPONENT,
                name: c"x",
                data_type: DOUBLE,
                rank: -1,
                access: server::READ,
                value: None,
            });
        }
        server.report_points(most);
    };
    let rewrite = placed(move |message, _, place| {
        if empty(place) {
            emptied(message);
        }
    });
    walked(setup, rewrite, everything())
}

/// The pages of places 1 to 6000 and 6002 to 12 001 have no references. At the bound of
/// 1, each open folder releases the point of `root`, and its cursor goes on as a held
/// one does, so the walk gives what the walk with no bound gives.
#[test]
fn a_walk_at_the_bound_past_pages_with_no_references_gives_what_an_unbound_walk_gives()
{
    let empty = |place| (1..=6000).contains(&place) || (6002..=12_001).contains(&place);
    let unbound = with_variables(0, empty).expect("found");
    assert_eq!(unbound.entries.len(), 2, "{unbound:?}");
    assert_eq!(unbound.end, End::Budget);
    let bound = with_variables(1, empty).expect("found");
    assert_eq!((bound.entries, bound.end), (unbound.entries, unbound.end));
}

/// The pages of places 1 to 4000, 4002 to 8001, and 8003 to 12 001 have no references.
/// A released cursor past pages with no references does not give those pages again,
/// so the walk at the bound of 1 gives what the walk with no bound gives.
#[test]
fn a_released_cursor_does_not_give_its_pages_with_no_references_again() {
    let empty = |place| {
        (1..=4000).contains(&place)
            || (4002..=8001).contains(&place)
            || (8003..=12_001).contains(&place)
    };
    let unbound = with_variables(0, empty).expect("found");
    assert_eq!(unbound.entries.len(), 3, "{unbound:?}");
    let bound = with_variables(1, empty).expect("found");
    assert_eq!((bound.entries, bound.end), (unbound.entries, unbound.end));
}

/// 12 003 references of `root` in a page each, the pages of places 1 to 12 001 with no
/// references. The relay frees the point of `root` at the page of place 6000, and the
/// walk browses `root` again from its start and ends at its budget, as a walk with a
/// held point does.
#[test]
fn a_walk_whose_point_the_server_freed_ends_at_its_budget() {
    let mut freed = false;
    let rewrite = placed(move |message, _, place| {
        if place == 6000 && !freed {
            assert_eq!(response(message), Some(BROWSE_NEXT_RESPONSE));
            freed = true;
            let invalid = Status::BAD_CONTINUATION_POINT_INVALID.0.to_le_bytes();
            message[56..60].copy_from_slice(&invalid);
        } else if (1..=12_001).contains(&place) {
            emptied(message);
        }
    });
    let found = walked(numbered(12_003), rewrite, everything());
    assert_eq!(found.expect("found").end, End::Budget);
}

/// 6003 references of `root` in a page each, the pages of places 1 to 6000 with no
/// references. The relay frees the point of `root` at the page of place 5000, and the
/// walk does not get the pages before it again, so it ends as a walk with a held point
/// does.
#[test]
fn a_walk_whose_point_the_server_freed_does_not_give_its_empty_pages_again() {
    let mut freed = false;
    let rewrite = placed(move |message, _, place| {
        if place == 5000 && !freed {
            freed = true;
            let invalid = Status::BAD_CONTINUATION_POINT_INVALID.0.to_le_bytes();
            message[56..60].copy_from_slice(&invalid);
        } else if (1..=6000).contains(&place) {
            emptied(message);
        }
    });
    let found = walked(numbered(6_003), rewrite, everything());
    assert_eq!(found.expect("found").end, End::Done);
}

/// 10 003 references of `root` in a page each, the pages of places 1 to 10 001 with no
/// references. At the bound of 1, the Browse of `n00000` releases the point of `root`,
/// and the walk still ends at its budget.
#[test]
fn a_walk_at_the_bound_ends_at_its_budget_past_pages_with_no_references() {
    for most in [0, 1] {
        let rewrite = placed(|message, _, place| {
            if (1..=10_001).contains(&place) {
                emptied(message);
            }
        });
        let setup = move |server: &Server| {
            numbered(10_003)(server);
            server.report_points(most);
        };
        let found = walked(setup, rewrite, everything());
        assert_eq!(found.expect("found").end, End::Budget, "bound {most}");
    }
}

const FOLDERS: u32 = 30;
const CHILDREN: u32 = 145;

/// The node of the folder `f` of [`wide`].
fn wide_folder(f: u32) -> Node {
    Node {
        namespace: 1,
        key: Key::Number(8000 + f),
    }
}

/// [`FOLDERS`] folders of [`CHILDREN`] variables of 4096 bytes of tag and name, with
/// 16 references in each page of the server.
fn wide(server: &Server) {
    server.add_folder(&ROOT, &OBJECTS, ORGANIZES, c"root", c"Root");
    server.add_data_type(&DOUBLE, &BASE_DATA_TYPE, c"Double");
    for f in 0..FOLDERS {
        let name = CString::new(format!("f{f}")).expect("no NUL");
        server.add_folder(&wide_folder(f), &ROOT, ORGANIZES, &name, &name);
        for c in 0..CHILDREN {
            let number = 20_000 + f * CHILDREN + c;
            let name = CString::new(format!("{number:0>4084}")).expect("no NUL");
            server.add_variable(&Variable {
                node: Node {
                    namespace: 1,
                    key: Key::Number(number),
                },
                parent: wide_folder(f),
                reference: HAS_COMPONENT,
                name: &name,
                data_type: DOUBLE,
                rank: -1,
                access: server::READ,
                value: None,
            });
        }
    }
    server.cap_references(16);
}

/// A walk of paths of [`wide`] picks a variable of the ninth page of each folder, so
/// it stops each folder with 576 KiB held, 16.9 MiB in all.
#[test]
fn a_walk_of_paths_releases_the_points_of_stopped_folders_past_the_bytes() {
    let (out, _) = run(0, async move |node| {
        serve(&node, 4, async |mut peer| {
            wide(peer.server);
            let mut client = peer.connected().await;
            let mut picks = Vec::new();
            {
                let mut server = Browse::new(&mut client);
                for f in 0..FOLDERS {
                    let at = entry(
                        &format!("root/f{f}"),
                        &wide_folder(f).to_string(),
                        Shape::Folder,
                    );
                    let (mut names, mut cursor) = (Vec::new(), None);
                    loop {
                        let page = server.level(Some(&at), cursor.as_ref()).await;
                        let page = page.expect("a page").expect("a folder");
                        names.extend(page.entries.into_iter().map(|c| c.name));
                        cursor = page.next;
                        if cursor.is_none() {
                            break;
                        }
                    }
                    assert_eq!(names.len(), 145);
                    let path = format!("root/f{f}/{}", names[143]);
                    picks.push(path.parse::<browse::Path>().expect("a path"));
                }
            }
            let query = Query {
                start: "root".parse().expect("a path"),
                scope: Scope::Points(picks),
                values: Values::Skipped,
            };
            let found =
                browse::walk(&mut Browse::new(&mut client), &query, &node.clock())
                    .await;
            client.close().await;
            found
        })
        .await
    });
    let found = out.expect("found");
    assert_eq!(found.entries.len(), 30);
    assert_eq!(found.end, End::Done);
}

/// A match of `root/*` over 600 points, one reference a page, on a link of 70 ms,
/// whose call of a page still runs at the deadline. It drops the call and reads the
/// values of the points that it found.
#[test]
fn a_match_whose_call_runs_at_the_deadline_reads_the_values_that_it_found() {
    let mut sim = sim::Sim::new(sim::Config {
        link: sim::link::Config {
            delay: types::time::Span::from_nanos(70_000_000),
            ..sim::link::Config::default()
        },
        steps_max: u64::MAX,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    let (found, took) = sim
        .run_on(&node, move |node, _| async move {
            serve(&node, 4, async |mut peer| {
                let variables = (0..600)
                    .map(|i| {
                        let node = Node {
                            namespace: 1,
                            key: Key::Number(20_000 + i),
                        };
                        let name = CString::new(format!("v{i:03}")).expect("no NUL");
                        (node, name, Some(f64::from(i)))
                    })
                    .collect();
                tree(variables)(peer.server);
                peer.server.cap_references(1);
                let mut client = peer.connected().await;
                let query = Query {
                    start: "root".parse().expect("a path"),
                    scope: Scope::Match("*".parse().expect("a pattern")),
                    values: Values::Read,
                };
                let clock = node.clock();
                let start = clock.now();
                let found =
                    browse::walk(&mut Browse::new(&mut client), &query, &clock).await;
                let took = clock.now() - start;
                client.close().await;
                (found, took)
            })
            .await
        })
        .expect("the run ends");
    let found = found.expect("found");
    let values: Vec<_> = found
        .entries
        .iter()
        .map(|entry| {
            let name = entry.path.to_string();
            let number: u32 = name["root/v".len()..].parse().expect("a number");
            Some(sample::Value::F64(f64::from(number)))
        })
        .collect();
    assert_eq!(found.values, values);
    assert_eq!((found.entries.len(), found.end), (140, End::Deadline));
    assert_eq!(took.nanos(), browse::DEADLINE.nanos() + 140_000_000);
}

/// Each browse of the kind of a cursor past 250 folders, one reference a page, on a
/// link of 200 ms, drops its call of `level` at the deadline, and the server keeps the
/// point of that call until its session closes. Each browse closes its session, so
/// over 33 such browses, past the 32 points that the server holds for a session, no
/// session holds more points than one browse, and the next browse gets its page. It
/// counts the points, not `BadNoContinuationPoints`: a full session of open62541
/// reclaims its oldest point, so a shared session also gets its page.
#[test]
fn browses_of_the_kind_that_drop_a_call_hold_no_point_of_an_earlier_browse() {
    let mut sim = sim::Sim::new(sim::Config {
        link: sim::link::Config {
            delay: types::time::Span::from_nanos(200_000_000),
            ..sim::link::Config::default()
        },
        steps_max: u64::MAX,
        ..sim::Config::default()
    });
    let node = sim.node(sim::node::Config::default());
    let (ends, last, points) = sim
        .run_on(&node, move |node, tasks| async move {
            let rig = rig(&node, tasks).await;
            serve(&node, 4, async |peer| {
                numbered(300)(peer.server);
                let count = peer.server.count();
                let config = config(&peer.url);
                let browse = async |scope| {
                    let query = Query {
                        start: "root".parse().expect("a path"),
                        scope,
                        values: Values::Skipped,
                    };
                    let found = rig.supervisor().browse("opcua", &config, &query).await;
                    found
                        .map(|found| (found.entries.len(), found.end))
                        .map_err(|error| error.to_string())
                };
                let mut ends = Vec::new();
                for _ in 0..33 {
                    let cursor = Cursor::new("250".to_owned());
                    ends.push(browse(Scope::Level(Some(cursor))).await);
                }
                let last = browse(Scope::Level(None)).await;
                (ends, last, count.points())
            })
            .await
        })
        .expect("the run ends");
    assert_eq!(ends, vec![Ok((0, End::Deadline)); 33]);
    assert_eq!(last, Ok((1, End::Next(Cursor::new("1".to_owned())))));
    assert_eq!(points, 1);
}
