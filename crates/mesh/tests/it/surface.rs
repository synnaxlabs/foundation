//! What a crate outside `mesh` reads of a region, as `hub` does.

use std::path::PathBuf;

use mesh::{Member, Mesh, Stopped, Watch, change, log};
use raft::{Position, Term};
use types::{channel, node};

type Home = Result<Option<node::Key>, Stopped>;

fn assert_gives_a_home<'a, F: Future<Output = Home>>(_: fn(&'a mut Watch) -> F) {}

fn assert_error<E: std::error::Error>(_: &E) {}

#[test]
fn watch_member_and_next_have_the_signatures_that_a_caller_holds() {
    let _: fn(&Mesh, channel::Key) -> Watch = Mesh::watch;
    let _: fn(&Mesh, node::Key) -> Option<Member> = Mesh::member;
    assert_gives_a_home(Watch::next);
}

#[test]
fn a_stop_names_its_cause() {
    let at = Position {
        term: Term(2),
        index: 3,
    };
    let unknown = Stopped::Change {
        at,
        cause: change::Unknown::Kind { kind: 9 },
    };
    assert_error(&unknown);
    assert_eq!(
        unknown.to_string(),
        "the committed entry at index 3 of term 2 is not a change: change kind 9 is \
         unknown"
    );
    let empty = Stopped::Change {
        at,
        cause: change::Unknown::Empty,
    };
    assert_eq!(
        empty.to_string(),
        "the committed entry at index 3 of term 2 is not a change: a change of 0 bytes \
         has no kind"
    );
    let write = Stopped::Write(log::Error::Corrupt {
        path: PathBuf::from("log-0"),
        offset: 512,
    });
    assert_eq!(
        write.to_string(),
        "the record at byte 512 of log-0 is not valid, and it is not a torn end of the \
         log"
    );
    assert_eq!(
        Stopped::Dropped.to_string(),
        "each mesh of the group dropped"
    );
}
