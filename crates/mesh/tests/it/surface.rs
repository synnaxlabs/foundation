//! What a crate outside `mesh` reads of a region, as `hub` does.

use std::path::PathBuf;

use mesh::{Error, Member, Mesh, Stopped, Watch, change, claim, log, region};
use raft::{Position, Term};
use types::{channel, node};

type Home = Result<Option<node::Key>, Error>;

fn assert_gives_a_home<'a, F: Future<Output = Home>>(_: fn(&'a mut Watch) -> F) {}

#[test]
fn a_mesh_gives_a_watch_of_a_home_and_the_record_of_a_member() {
    let _: fn(&Mesh, channel::Key) -> Watch = Mesh::watch;
    let _: fn(&Mesh, node::Key) -> Option<Member> = Mesh::member;
    assert_gives_a_home(Watch::next);
}

#[test]
fn an_error_names_the_cause_that_its_module_gives() {
    let key = node::Key::from_u128(2);
    let log = log::Error::Corrupt {
        path: PathBuf::from("log-0"),
        offset: 512,
    };
    assert_eq!(
        Error::from(log).to_string(),
        "the record at byte 512 of log-0 is not valid, and it is not a torn end of the \
         log"
    );
    assert_eq!(
        Error::from(claim::Error::Forged { signer: key }).to_string(),
        format!("the claim of node {key} is forged")
    );
    assert_eq!(
        Error::from(claim::Error::NotMember { signer: key }).to_string(),
        format!("node {key} is not a member of the region")
    );
    assert_eq!(
        Error::Member(region::Unfit::Duplicate { key }).to_string(),
        format!("node {key} is already a member")
    );
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
    assert_eq!(
        Error::Stopped(unknown).to_string(),
        "the group stopped: the committed entry at index 3 of term 2 is not a change: \
         change kind 9 is unknown"
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
    assert_eq!(
        Error::Stopped(Stopped::Dropped).to_string(),
        "the group stopped: each mesh of the group dropped"
    );
}
