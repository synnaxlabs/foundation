//! What a crate outside `mesh` reads of a region, as `hub` does.

use mesh::{Member, Mesh, Watch};
use types::{channel, node};

#[test]
fn a_mesh_gives_a_watch_of_a_home_and_the_record_of_a_member() {
    let _: fn(&Mesh, channel::Key) -> Watch = Mesh::watch;
    let _: fn(&Mesh, node::Key) -> Option<Member> = Mesh::member;
}
