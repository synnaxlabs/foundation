//! The tree that [`super::serve`] serves, as the module doc of [`super`] gives it.

use std::ffi::CStr;

use crate::ffi::UNIX_EPOCH;
use crate::node::ns0::{
    self, BASE_DATA_TYPE, BYTE_STRING, DATE_TIME, DOUBLE, ENUMERATION, HAS_COMPONENT,
    OBJECTS, ORGANIZES, STRING, STRUCTURE,
};
use crate::node::{Key, Name, Node};
use crate::server::{self, READ, Server, Variable};
use crate::value::Value;

/// The most references of each page of a Browse, so `Plant.Types` takes two pages.
pub(crate) const REFERENCES: u32 = 8;

/// The path of each node of the tree, its name in namespace 1.
pub(crate) const PLANT: &str = "Plant";
pub(crate) const TYPES: &str = "Plant.Types";
pub(crate) const LINE: &str = "Plant.Line";
pub(crate) const TEMPERATURE: &str = "Plant.Line.Temperature";
pub(crate) const UNIT: &str = "Plant.Line.Temperature.EngineeringUnits";
pub(crate) const SPEED: &str = "Plant.Line.Speed";
pub(crate) const MODE: &str = "Plant.Line.Mode";
pub(crate) const PROFILE: &str = "Plant.Line.Profile";
pub(crate) const LOCKED: &str = "Plant.Line.Locked";
pub(crate) const RECIPE: &str = "Plant.Line.Recipe";
pub(crate) const ODD: &str = "Plant.Odd";
pub(crate) const LEVEL: &str = "Plant.Odd.Level";
pub(crate) const TWIN: &str = "Plant.Odd.Twin";

/// The name of each data type of the tree in namespace 1.
pub(crate) const SPEED_TYPE: &str = "Speed";
pub(crate) const MODE_TYPE: &str = "Mode";
pub(crate) const RECIPE_TYPE: &str = "Recipe";

/// The node id of the folder `Plant.Odd.guid`.
#[cfg(test)]
pub(crate) const GUID: &str = "ns=1;g=00000001-0002-0003-0400-000000000000";

/// The number in namespace 0 of each type that the tree adds, since the namespace 0
/// of the build has none of them.
#[cfg(test)]
pub(crate) const NS0_TYPES: [u32; 3] = [DOUBLE, ENUMERATION, STRUCTURE];

/// Gives the node of `name` in namespace 1.
///
/// # Panics
///
/// If `name` is empty.
pub(crate) fn node(name: &str) -> Node {
    let name = Name::new(name).expect("invariant: a node of the tree has a name");
    Node {
        namespace: 1,
        key: Key::Name(name),
    }
}

/// Adds the tree to `server`.
///
/// # Panics
///
/// If open62541 refuses a node of the tree, such as one that the server has.
pub(crate) fn add(server: &Server) {
    let objects = Node::ns0(OBJECTS);
    server.add_folder(&node(PLANT), &objects, ORGANIZES, c"Plant", c"Plant");
    add_types(server);
    add_line(server);
    add_odd(server);
}

/// Adds `Plant.Types` and a variable of each type.
fn add_types(server: &Server) {
    server.add_folder(&node(TYPES), &node(PLANT), ORGANIZES, c"Types", c"Types");
    let numbers = [
        (c"Boolean", Value::Bool(true)),
        (c"SByte", Value::I8(-8)),
        (c"Byte", Value::U8(8)),
        (c"Int16", Value::I16(-16)),
        (c"UInt16", Value::U16(16)),
        (c"Int32", Value::I32(-32)),
        (c"UInt32", Value::U32(32)),
        (c"Int64", Value::I64(-64)),
        (c"UInt64", Value::U64(64)),
        (c"Float", Value::F32(0.5)),
        (c"Double", Value::F64(0.25)),
    ]
    .map(|(name, value)| (name, server::variant(value)));
    let text = [&4_i32.to_le_bytes()[..], b"text"].concat();
    // The DateTime of the Unix epoch, in 100 ns since 1601.
    let epoch = UNIX_EPOCH.to_le_bytes().to_vec();
    let guid = [
        &1_u32.to_le_bytes()[..],
        &[2, 0, 3, 0, 4, 0, 0, 0, 0, 0, 0, 0],
    ]
    .concat();
    let others = [
        (c"String", STRING, text),
        (c"DateTime", DATE_TIME, epoch),
        (c"Guid", ns0::GUID, guid),
        (c"ByteString", BYTE_STRING, two_bytes()),
    ]
    .map(|(name, id, encoded)| (name, (id, server::scalar(id, &encoded))));
    for (name, (id, value)) in numbers.into_iter().chain(others) {
        let name_text = name
            .to_str()
            .expect("invariant: each name of the tree is ASCII");
        let path = format!("{TYPES}.{name_text}");
        server.add_variable(&variable(&path, TYPES, name, Node::ns0(id), Some(value)));
    }
}

/// Adds the data types of the tree, `Plant.Line`, and its components.
fn add_line(server: &Server) {
    let base = Node::ns0(BASE_DATA_TYPE);
    let (double, enumeration, structure) = (
        Node::ns0(DOUBLE),
        Node::ns0(ENUMERATION),
        Node::ns0(STRUCTURE),
    );
    server.add_data_type(&double, &base, c"Double");
    server.add_data_type(&enumeration, &base, c"Enumeration");
    server.add_data_type(&structure, &base, c"Structure");
    server.add_data_type(&node(SPEED_TYPE), &double, c"Speed");
    server.add_data_type(&node(MODE_TYPE), &enumeration, c"Mode");
    server.add_data_type(&node(RECIPE_TYPE), &structure, c"Recipe");
    let line = node(LINE);
    server.add_folder(&line, &node(PLANT), HAS_COMPONENT, c"Line", c"Line");
    server.add_reference(&line, ORGANIZES, &node(PLANT));
    let temperature = Some(double_of(21.5));
    let name = c"Temperature";
    server.add_variable(&variable(
        TEMPERATURE,
        LINE,
        name,
        double.clone(),
        temperature,
    ));
    let cefact = c"http://www.opcfoundation.org/UA/units/un/cefact";
    server.add_unit(&node(UNIT), &node(TEMPERATURE), cefact, c"°C", 0x0043_454C);
    let speed = Some(double_of(1.25));
    server.add_variable(&variable(SPEED, LINE, c"Speed", node(SPEED_TYPE), speed));
    // open62541 refuses an Int32 value of a subtype of Enumeration.
    server.add_variable(&variable(MODE, LINE, c"Mode", node(MODE_TYPE), None));
    let profile = Some(server::doubles());
    server.add_variable(&Variable {
        rank: 1,
        ..variable(PROFILE, LINE, c"Profile", double.clone(), profile)
    });
    server.add_variable(&Variable {
        access: 0,
        ..variable(LOCKED, LINE, c"Locked", double, None)
    });
    // An ExtensionObject of the type `Recipe`: its NodeId, then a ByteString body.
    let name = RECIPE_TYPE.as_bytes();
    let length =
        i32::try_from(name.len()).expect("invariant: a type name of the tree is short");
    let recipe = [
        &[3, 1, 0][..],
        &length.to_le_bytes(),
        name,
        &[1],
        &two_bytes(),
    ]
    .concat();
    let recipe = server::scalar(STRUCTURE, &recipe);
    server.add_variable(&variable(
        RECIPE,
        LINE,
        c"Recipe",
        node(RECIPE_TYPE),
        Some(recipe),
    ));
}

/// Adds `Plant.Odd` and its nodes.
fn add_odd(server: &Server) {
    let odd = node(ODD);
    server.add_folder(&odd, &node(PLANT), ORGANIZES, c"Odd", c"Odd");
    server.add_guid(&odd);
    let double = Node::ns0(DOUBLE);
    let level = Some(double_of(2.5));
    server.add_variable(&variable(LEVEL, ODD, c"Level", double.clone(), level));
    server.add_reference(&odd, ORGANIZES, &node(LEVEL));
    server.add_variable(&variable(TWIN, ODD, c"Level", double, Some(double_of(3.5))));
}

/// The scalar variable `path` of `parent` by `HasComponent`, with the browse name
/// `name`, the `DataType` `data_type`, the access read, and the value `value`.
fn variable(
    path: &str,
    parent: &str,
    name: &'static CStr,
    data_type: Node,
    value: Option<Vec<u8>>,
) -> Variable<'static> {
    Variable {
        node: node(path),
        parent: node(parent),
        reference: HAS_COMPONENT,
        name,
        data_type,
        rank: -1,
        access: READ,
        value,
    }
}

/// The encoded `Variant` of the Double `double`.
fn double_of(double: f64) -> Vec<u8> {
    let (_, value) = server::variant(Value::F64(double));
    value
}

/// The encoding of the `ByteString` 1, 2.
fn two_bytes() -> Vec<u8> {
    [&2_i32.to_le_bytes()[..], &[1, 2]].concat()
}
