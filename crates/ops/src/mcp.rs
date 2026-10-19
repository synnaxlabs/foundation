use std::io::{self, BufRead, Read, Write};

use connector::kind::Table;
use serde_json::{Map, Value, json};

use crate::error::Error;
use crate::operation::{self, TABLE};
use crate::remote::Link;
use crate::scope::Scope;
use crate::{Failure, Stop};

/// The one protocol version this server speaks. A client that asks for another gets
/// this one, and may then disconnect.
const VERSION: &str = "2025-06-18";

/// The guide for an agent, which `initialize` gives as its `instructions`. Each name
/// in backticks is a tool, an input of one, or the call `fill`.
pub(crate) const GUIDE: &str = "Run `docs` for the blocks and attributes that a config \
    file can hold.\n\
    To read the points of a device into Foundation:\n\
    1. Run `browse` on a connector in its config files, from the root or a `start`, \
    to find the paths of its points. The files must hold the connector block.\n\
    2. Run `import` with those paths as `points`, or with a `match` pattern, to add a \
    channel for each point to the config file of the connector.\n\
    3. Write a value in place of each `fill` that `import` names.\n\
    4. Run `plan` on the config files, and show the change to the person.\n\
    5. Run `apply` on the plan file that `plan` wrote to `out`, only when the person \
    agrees.\n\
    `status` shows the state of each connector.\n";

const PARSE: (i64, &str) = (-32700, "Parse error");
const REQUEST: (i64, &str) = (-32600, "Invalid Request");
const METHOD: (i64, &str) = (-32601, "Method not found");
const PARAMS: (i64, &str) = (-32602, "Invalid params");

/// The longest line, with its newline, that [`line`] reads into memory.
pub(crate) const LIMIT: usize = 1 << 20;

/// One line of the input of `foundation mcp`, as [`line`] reads it.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    /// A line, with its `\n` when it has one. Only the last line of the input can
    /// have none.
    Bytes(Vec<u8>),
    /// A line longer than 1 MiB with its `\n`. The read skips it up to its `\n`.
    Long,
    /// The input ended.
    End,
}

/// Reads the next line of `input`. It holds at most 1 MiB of it in memory.
///
/// # Errors
///
/// The first error of a read of `input`.
pub fn line(input: &mut impl BufRead) -> io::Result<Line> {
    let mut bytes = Vec::new();
    let read = input.take(LIMIT as u64).read_until(b'\n', &mut bytes)?;
    if read == 0 {
        Ok(Line::End)
    } else if read == LIMIT && !bytes.ends_with(b"\n") && !input.fill_buf()?.is_empty()
    {
        input.skip_until(b'\n')?;
        Ok(Line::Long)
    } else {
        Ok(Line::Bytes(bytes))
    }
}

/// Answers each line that `input` gives with at most one line on `output`, until
/// `input` ends or the reader closes `output`. A tool call reads and writes its files
/// through `scope`, and sends its request to the node through `link`. `docs` states
/// each kind of `kinds`.
pub(crate) async fn serve<R, C>(
    mut input: impl AsyncFnMut() -> io::Result<Line>,
    output: &mut impl Write,
    scope: &Scope,
    kinds: &Table,
    link: &mut Link<R, C>,
) -> Result<(), Stop>
where
    R: AsyncFnMut() -> Result<C, Failure>,
    C: AsyncFn(&[u8]) -> Result<Vec<u8>, Failure>,
{
    loop {
        let read = input().await.map_err(|e| Error::Input {
            message: e.to_string(),
        })?;
        let reply = match read {
            Line::End => return Ok(()),
            Line::Long => Some(reply(&Value::Null, Err(fault(REQUEST))).to_string()),
            // A line that is not UTF-8 is not JSON either, so "" gets the parse error.
            Line::Bytes(bytes) => {
                respond(
                    std::str::from_utf8(&bytes).unwrap_or(""),
                    scope,
                    kinds,
                    link,
                )
                .await
            }
        };
        if let Some(reply) = reply {
            crate::write(output, &format!("{reply}\n"))?;
        }
    }
}

/// Answers one MCP message, a JSON-RPC 2.0 request or notification. Returns the reply
/// as one line of JSON, or `None` for a notification or a response.
///
/// A request id must be a string or a 64-bit integer, so the reply carries it
/// unchanged. An unknown tool and a bad argument give a JSON-RPC error whose `data` is
/// `{"errors": [...]}`, each item a problem with `code`, `message`, `fix`, `place` when
/// the problem is in a file, and `notes`. A failed operation gives a result with
/// `isError` true and that JSON as its one text.
pub(crate) async fn respond<R, C>(
    message: &str,
    scope: &Scope,
    kinds: &Table,
    link: &mut Link<R, C>,
) -> Option<String>
where
    R: AsyncFnMut() -> Result<C, Failure>,
    C: AsyncFn(&[u8]) -> Result<Vec<u8>, Failure>,
{
    let reply = match serde_json::from_str::<Value>(message) {
        Ok(Value::Object(message)) => answer(message, scope, kinds, link).await?,
        Ok(_) => reply(&Value::Null, Err(fault(REQUEST))),
        Err(_) => reply(&Value::Null, Err(fault(PARSE))),
    };
    Some(reply.to_string())
}

async fn answer<R, C>(
    mut message: Map<String, Value>,
    scope: &Scope,
    kinds: &Table,
    link: &mut Link<R, C>,
) -> Option<Value>
where
    R: AsyncFnMut() -> Result<C, Failure>,
    C: AsyncFn(&[u8]) -> Result<Vec<u8>, Failure>,
{
    if !message.contains_key("method")
        && (message.contains_key("result") || message.contains_key("error"))
    {
        return None;
    }
    let id = match message.remove("id") {
        None => None,
        Some(id) if id.is_string() || id.is_i64() || id.is_u64() => Some(id),
        Some(_) => return Some(reply(&Value::Null, Err(fault(REQUEST)))),
    };
    let method = match message.remove("method") {
        Some(Value::String(method))
            if message.get("jsonrpc") == Some(&json!("2.0")) =>
        {
            method
        }
        _ => return Some(reply(&id.unwrap_or(Value::Null), Err(fault(REQUEST)))),
    };
    // Checked after the request itself, so an invalid notification gets a reply.
    let id = id?;
    let Some(method) = Method::of(&method) else {
        return Some(reply(&id, Err(fault(METHOD))));
    };
    let params = match message.remove("params") {
        None => Map::new(),
        Some(Value::Object(params)) => params,
        Some(_) => return Some(reply(&id, Err(fault(PARAMS)))),
    };
    let result = match method {
        Method::Initialize => Ok(initialize()),
        Method::Ping => Ok(json!({})),
        Method::List => Ok(tools()),
        Method::Call => call(params, scope, kinds, link).await,
    };
    Some(reply(&id, result))
}

/// The methods that the server answers.
enum Method {
    Initialize,
    Ping,
    List,
    Call,
}

impl Method {
    fn of(name: &str) -> Option<Self> {
        match name {
            "initialize" => Some(Self::Initialize),
            "ping" => Some(Self::Ping),
            "tools/list" => Some(Self::List),
            "tools/call" => Some(Self::Call),
            _ => None,
        }
    }
}

fn initialize() -> Value {
    json!({
        "protocolVersion": VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "foundation", "version": env!("CARGO_PKG_VERSION") },
        "instructions": GUIDE,
    })
}

fn tools() -> Value {
    let inputs = operation::inputs();
    let outputs = operation::outputs();
    let tools: Vec<Value> = TABLE
        .iter()
        .map(|spec| {
            json!({
                "name": spec.name,
                "description": spec.summary,
                "inputSchema": inputs[spec.name],
                "outputSchema": outputs[spec.name],
                "annotations": {
                    "readOnlyHint": spec.read_only,
                    "destructiveHint": spec.destructive,
                },
            })
        })
        .collect();
    json!({ "tools": tools })
}

async fn call<R, C>(
    mut params: Map<String, Value>,
    scope: &Scope,
    kinds: &Table,
    link: &mut Link<R, C>,
) -> Result<Value, Value>
where
    R: AsyncFnMut() -> Result<C, Failure>,
    C: AsyncFn(&[u8]) -> Result<Vec<u8>, Failure>,
{
    let Some(Value::String(name)) = params.remove("name") else {
        return Err(fault(PARAMS));
    };
    let arguments = match params.remove("arguments") {
        None => Map::new(),
        Some(Value::Object(arguments)) => arguments,
        Some(_) => return Err(fault(PARAMS)),
    };
    match operation::read(&name, arguments) {
        Ok(request) => Ok(match request.run(scope, kinds, link).await {
            Ok(response) => {
                let content = response.json();
                json!({
                    "content": [{ "type": "text", "text": content.to_string() }],
                    "structuredContent": content,
                })
            }
            Err(error) => json!({
                "content": [{ "type": "text", "text": error.json().to_string() }],
                "isError": true,
            }),
        }),
        Err(error) => Err(json!({
            "code": PARAMS.0,
            "message": error.to_string(),
            "data": error.json(),
        })),
    }
}

fn fault((code, message): (i64, &str)) -> Value {
    json!({ "code": code, "message": message })
}

fn reply(id: &Value, result: Result<Value, Value>) -> Value {
    match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
    }
}
