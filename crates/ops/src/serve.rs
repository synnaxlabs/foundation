//! The requests and replies of a remote program, and how the node runs each request.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};
use std::marker::PhantomData;

use access::Rules;
use serde::de::value::MapAccessDeserializer;
use serde::de::{self, DeserializeOwned, Deserializer, IgnoredAny, MapAccess};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use spec::definition::{Definition, Kind};
use types::name::{Name, Prefix};
use wire::hub::client::BODY_BYTES_MAX;

use crate::error::{Action, Error, On, Problem};
use crate::front_end::File;
use crate::operation::Empty;
use crate::plan::{Documents, Output};
use crate::{Node, apply, status, used};

#[cfg(test)]
mod tests;

/// Runs the request `body` of `subject` on `node`, and gives the body of the reply:
/// the output of the operation, or its error with code, message, and fix. `body` is
/// what [`cli`] sends. Call it only for a subject whose key the hub verified.
///
/// The reply is at most `wire::hub::client::BODY_BYTES_MAX` bytes, and `serve` stops
/// writing a reply at that bound. No `body` makes it panic. The reply holds
/// `ops.bad-request` for a body that is no request of `plan`, `apply`, or `status`,
/// `ops.denied` for an action that no access policy gives `subject` and for a request
/// that names no definition, and `ops.large-reply` in place of a reply over the bound.
/// `apply` also needs `apply` on the channel at each edge of each channel that the plan
/// adds or changes, and `admin` on the key of each definition other than a channel that
/// the plan adds, changes, or removes. It gives a home only to an index on which
/// `subject` has `apply`, and drops each other home with no error. `status` shows only
/// each connector on whose label and status channels `subject` has `read`. When it
/// shows none, also when the node places none, it needs `read` on the name of the
/// node, and gives `ops.denied` with no grant. It gives the failures of the read of the
/// OS roots of the node only to a subject with `read` on the name of the node. It
/// waits at most 1 s from the request. A connector is `unknown` when no status as the
/// spec defines it comes in that time, and `status` gives `ops.channel-read` when the
/// reader of a connector fails for another cause.
///
/// [`cli`]: crate::cli
pub async fn serve(node: &Node, subject: &Name, body: &[u8]) -> Vec<u8> {
    let reply = run(node, subject, body).await;
    reply.unwrap_or_else(|error| failed(&error)).0
}

/// A body that would pass the bound.
#[derive(Debug)]
struct Large;

impl From<Large> for Error {
    fn from(Large: Large) -> Self {
        Self::LargeReply
    }
}

/// The bytes of a request or a reply: at most `BODY_BYTES_MAX`.
#[derive(Default)]
struct Body(Vec<u8>);

impl Body {
    /// The body of `head` as one line of JSON. Stops at the bound.
    ///
    /// # Errors
    ///
    /// `Large` when the line passes the bound.
    fn line(head: &impl Serialize) -> Result<Self, Large> {
        let mut body = Self::default();
        match serde_json::to_writer(&mut body, head) {
            Ok(()) => {}
            Err(error) if error.is_io() => return Err(Large),
            Err(error) => panic!("invariant: a body is plain JSON data: {error}"),
        }
        body.add(b"\n")?;
        Ok(body)
    }

    /// Adds `bytes` to the end.
    ///
    /// # Errors
    ///
    /// `Large`, with no byte added, when the body would pass the bound.
    fn add(&mut self, bytes: &[u8]) -> Result<(), Large> {
        let len = self.0.len().saturating_add(bytes.len());
        if u64::try_from(len).is_ok_and(|len| len <= BODY_BYTES_MAX) {
            self.0.extend_from_slice(bytes);
            Ok(())
        } else {
            Err(Large)
        }
    }
}

impl Write for Body {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.add(bytes)
            .map(|()| bytes.len())
            .map_err(|Large| io::ErrorKind::FileTooLarge.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The first line of a request: one operation and its input. The bytes of a plan
/// file follow the line of `apply`, as they are.
enum Request {
    Plan(Plan),
    Apply,
    Status,
    /// An operation of another name.
    Other(String),
}

/// The input of `plan`: each config file, with its path as the user gave it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    #[serde(deserialize_with = "objects")]
    files: Vec<File>,
}

/// A `T` that takes only an object: a derived struct also takes a sequence.
struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T>(PhantomData<T>);

        impl<'de, T: Deserialize<'de>> de::Visitor<'de> for Visitor<T> {
            type Value = Object<T>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object")
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                map: A,
            ) -> Result<Object<T>, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(Object)
            }
        }

        deserializer.deserialize_map(Visitor(PhantomData))
    }
}

/// The files of a [`Plan`], each only from an object.
fn objects<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<File>, D::Error> {
    let files = Vec::<Object<File>>::deserialize(deserializer)?;
    Ok(files.into_iter().map(|Object(file)| file).collect())
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = Request;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object with one operation")
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Request, A::Error> {
                let Some(name) = map.next_key::<String>()? else {
                    return Err(de::Error::custom("no operation"));
                };
                let request = match name.as_str() {
                    "plan" => {
                        let Object(plan) = map.next_value()?;
                        Request::Plan(plan)
                    }
                    "apply" => {
                        let Object(Empty {}) = map.next_value()?;
                        Request::Apply
                    }
                    "status" => {
                        let Object(Empty {}) = map.next_value()?;
                        Request::Status
                    }
                    _ => {
                        let IgnoredAny = map.next_value()?;
                        Request::Other(name)
                    }
                };
                match map.next_key::<IgnoredAny>()? {
                    Some(IgnoredAny) => Err(de::Error::custom("a second operation")),
                    None => Ok(request),
                }
            }
        }

        deserializer.deserialize_map(Visitor)
    }
}

/// The first line of a reply: the output of an operation that ran, or its errors with
/// the exit status of the program.
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum Answer<'a, T> {
    Done {
        output: T,
    },
    Failed {
        errors: Cow<'a, [Problem]>,
        status: u8,
    },
}

/// The reply that holds `error`, or `ops.large-reply` when that reply passes the bound.
fn failed(error: &Error) -> Body {
    let line = |error: &Error| {
        Body::line(&Answer::<()>::Failed {
            errors: error.problems(),
            status: error.status(),
        })
    };
    line(error).unwrap_or_else(|Large| {
        line(&Error::LargeReply).unwrap_or_else(|Large| {
            panic!("invariant: the reply of `ops.large-reply` fits")
        })
    })
}

/// The body of a request of `plan` of `files`, for [`serve`].
///
/// # Errors
///
/// [`Error::LargeRequest`] when the body passes the bound.
///
/// # Panics
///
/// When the path of a file is not UTF-8 and the body before that path fits the bound.
pub(crate) fn plan_request(files: &[File]) -> Result<Vec<u8>, Error> {
    let plan = BTreeMap::from([("files", files)]);
    let body = Body::line(&BTreeMap::from([("plan", plan)]));
    body.map(|body| body.0).map_err(|Large| Error::LargeRequest)
}

/// The body of a request of `apply` of the plan file `plan`, for [`serve`].
///
/// # Errors
///
/// [`Error::LargeRequest`] when the body passes the bound.
pub(crate) fn apply_request(plan: &[u8]) -> Result<Vec<u8>, Error> {
    let large = |Large| Error::LargeRequest;
    let mut body = Body::line(&json!({ "apply": {} })).map_err(large)?;
    body.add(plan).map_err(large)?;
    Ok(body.0)
}

/// The body of a request of `status`, for [`serve`].
pub(crate) const STATUS_REQUEST: &[u8] = b"{\"status\":{}}\n";

/// The output of the reply `body` of [`serve`] to a request of `plan`, and the plan
/// file after its first line.
///
/// # Errors
///
/// Each error of [`answer`], and [`Error::Reply`] when no plan follows the output.
pub(crate) fn plan_reply(body: &[u8]) -> Result<(Output, &[u8]), Error> {
    let (output, plan) = answer(body)?;
    if plan.is_empty() {
        return Err(bad_reply(
            "no plan follows the first line of the reply of `plan`",
        ));
    }
    Ok((output, plan))
}

/// The output of the reply `body` of [`serve`] to a request of `operation`.
///
/// # Errors
///
/// Each error of [`answer`], and [`Error::Reply`] when bytes follow the output.
pub(crate) fn reply<T: DeserializeOwned>(
    body: &[u8],
    operation: &str,
) -> Result<T, Error> {
    let (output, rest) = answer(body)?;
    if !rest.is_empty() {
        return Err(bad_reply(format!(
            "bytes follow the first line of the reply of `{operation}`"
        )));
    }
    Ok(output)
}

/// The output of the reply `body` of [`serve`], and the bytes after its first line.
///
/// # Errors
///
/// - [`Error::Remote`] with the errors of the reply and its exit status.
/// - [`Error::Reply`] when `body` is no reply, or holds no error with a status of 0.
fn answer<T: DeserializeOwned>(body: &[u8]) -> Result<(T, &[u8]), Error> {
    let Some((head, rest)) = first_line(body) else {
        return Err(bad_reply("no newline ends the first line"));
    };
    match serde_json::from_slice::<Answer<'_, Value>>(head) {
        Ok(Answer::Done { output }) => match T::deserialize(output) {
            Ok(output) => Ok((output, rest)),
            Err(error) => Err(bad_reply(error.to_string())),
        },
        Ok(Answer::Failed { errors, status }) if status == 0 || errors.is_empty() => {
            Err(bad_reply("an error has the exit status 0, or no problem"))
        }
        Ok(Answer::Failed { errors, status }) => Err(Error::Remote {
            problems: errors.into_owned(),
            status,
        }),
        Err(error) => Err(bad_reply(error.to_string())),
    }
}

/// The first line of `body` and the bytes after its newline, or `None` when no newline
/// ends it.
fn first_line(body: &[u8]) -> Option<(&[u8], &[u8])> {
    let end = body.iter().position(|byte| *byte == b'\n')?;
    Some((&body[..end], &body[end + 1..]))
}

/// `Request` with `message`.
fn bad_request(message: impl Into<String>) -> Error {
    Error::Request {
        message: message.into(),
    }
}

/// `Reply` with `message`.
fn bad_reply(message: impl Into<String>) -> Error {
    Error::Reply {
        message: message.into(),
    }
}

/// The reply of the request `body` of `subject`.
async fn run(node: &Node, subject: &Name, body: &[u8]) -> Result<Body, Error> {
    let Some((head, rest)) = first_line(body) else {
        return Err(bad_request("no newline ends the first line"));
    };
    let request =
        serde_json::from_slice(head).map_err(|e| bad_request(e.to_string()))?;
    match request {
        Request::Plan(_) if !rest.is_empty() => {
            Err(bad_request("bytes follow the first line of `plan`"))
        }
        Request::Plan(Plan { files }) => plan(node, subject, &files).await,
        Request::Apply => apply(node, subject, rest).await,
        Request::Status if !rest.is_empty() => {
            Err(bad_request("bytes follow the first line of `status`"))
        }
        Request::Status => status(node, subject).await,
        Request::Other(name) => Err(bad_request(format!(
            "the operation `{name}` does not run on a node"
        ))),
    }
}

/// The access check of one request: `subject` needs an action on each name that the
/// operation reads or changes, by the rules of the spec in use.
struct Check<'a> {
    rules: Rules,
    subject: &'a Name,
}

impl<'a> Check<'a> {
    /// The check of `subject` by the access policies of `spec`.
    fn new(spec: &used::Spec, subject: &'a Name) -> Self {
        Self {
            rules: Rules::new([(Prefix::ROOT, spec.definitions().iter())]),
            subject,
        }
    }

    /// Whether an access policy gives `action` on `name`.
    fn allows(&self, action: Action, name: &Name) -> bool {
        let grant = self.rules.grant(self.subject, name);
        grant.actions().contains(action.into())
    }

    /// `Denied` of `action` on the first of `names` on which no access policy gives
    /// it, or `Ok` when each has a grant.
    fn names<'n>(
        &self,
        action: Action,
        names: impl IntoIterator<Item = &'n Name>,
    ) -> Result<(), Error> {
        match names.into_iter().find(|name| !self.allows(action, name)) {
            Some(name) => Err(self.denied(action, On::Name(name.clone()))),
            None => Ok(()),
        }
    }

    /// `Denied` of `action` on `on`.
    fn denied(&self, action: Action, on: On) -> Error {
        Error::Denied {
            subject: self.subject.clone(),
            action,
            on,
        }
    }
}

/// The reply of `plan` of `files`: its output, then the plan file.
async fn plan(node: &Node, subject: &Name, files: &[File]) -> Result<Body, Error> {
    let files = Documents::read(files, &node.front_ends)?;
    let names = files.names(node.supervisor.kinds())?;
    let spec = used::spec(&node.mesh).await?;
    let check = Check::new(&spec, subject);
    check.names(Action::Plan, &names)?;
    let mut compared = spec
        .definitions()
        .iter()
        .filter(|(name, definition)| config::plan::planned(name, definition.kind()))
        .map(|(name, _)| name)
        .peekable();
    if names.is_empty() && compared.peek().is_none() {
        return Err(check.denied(Action::Plan, On::Nothing));
    }
    // The files do not hold this name, so the error does not tell what the spec holds.
    if compared.any(|name| !check.allows(Action::Plan, name)) {
        return Err(check.denied(Action::Plan, On::Spec));
    }
    let (base, applied) = spec.newest()?;
    // A subject that may not apply a connector learns nothing of its sources, and
    // `apply` of a connector needs `admin` too.
    let readable = |name: &Name| {
        check.allows(Action::Apply, name) && check.allows(Action::Admin, name)
    };
    let (output, plan) = files.plan(node, base, &applied, readable).await?;
    let mut reply = Body::line(&Answer::Done { output: &output })?;
    reply.add(&plan.encode())?;
    Ok(reply)
}

/// The reply of `apply` of the plan file `bytes`: its output.
async fn apply(node: &Node, subject: &Name, bytes: &[u8]) -> Result<Body, Error> {
    let mut plan = config::plan::Plan::decode(bytes).map_err(Error::Plan)?;
    let spec = used::spec(&node.mesh).await?;
    let check = Check::new(&spec, subject);
    check.names(Action::Apply, plan.changes.keys())?;
    let edges = plan.changes.values().filter_map(|change| {
        match &change.new.as_ref()?.definition {
            config::Definition::Channel(kind) => Some(kind),
            _ => None,
        }
    });
    check.names(
        Action::Apply,
        edges.flat_map(|kind| kind.edges().map(|(_, to)| to)),
    )?;
    // A policy governs its whole region, the keys of a subject decide who acts as it,
    // a region record makes a region, and a connector reaches channels by `select`,
    // so `apply` on a subtree would reach each name of the region.
    let admin = plan.changes.iter().filter_map(|(name, change)| {
        let stored = spec.definitions().get(name).map(Definition::kind);
        let new = change.new.as_ref().map(|entry| entry.definition.kind());
        // A connector and a channel share a key, so a change can turn one into the
        // other.
        let exempt = matches!(
            (stored, new),
            (Some(Kind::Channel) | None, Some(Kind::Channel))
                | (Some(Kind::Channel), None)
        );
        (!exempt).then_some(name)
    });
    check.names(Action::Admin, admin)?;
    // The homes list each placed index, so a home with no grant drops with no error.
    plan.homes
        .retain(|index, _| check.allows(Action::Apply, index));
    // Else a subject with no grant learns the pointer in use from the output.
    if plan.changes.is_empty() && plan.homes.is_empty() {
        return Err(check.denied(Action::Apply, On::Nothing));
    }
    let effect =
        apply::apply(plan, spec, &node.mesh, node.supervisor.kinds(), &node.keys)
            .await?;
    Ok(Body::line(&Answer::Done { output: &effect })?)
}

/// The reply of `status`: its output. It leaves out each connector unless `subject`
/// has `read` on its label and on each of its status channels. When it leaves out each
/// connector, or the node places none, it needs `read` on the name of the node. It
/// gives the failures of the OS roots only with `read` on the name of the node.
async fn status(node: &Node, subject: &Name) -> Result<Body, Error> {
    let asked = node.clock.now();
    let spec = used::spec(&node.mesh).await?;
    let check = Check::new(&spec, subject);
    let mut placed = status::placed(spec.definitions(), &node.name);
    placed.retain(|placed| {
        check.allows(Action::Read, placed.name())
            && placed
                .channels()
                .all(|name| check.allows(Action::Read, name))
    });
    let readable = check.allows(Action::Read, &node.name);
    let connectors = if placed.is_empty() {
        if !readable {
            return Err(check.denied(Action::Read, On::Nothing));
        }
        Vec::new()
    } else {
        status::read(node, subject, &placed, asked).await?
    };
    let output = status::Output {
        connectors,
        roots: readable.then(|| node.roots.clone()),
    };
    Ok(Body::line(&Answer::Done { output: &output })?)
}
