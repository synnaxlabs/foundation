use std::io::{self, BufRead, Read, Write};

use serde_json::{Map, Value, json};

use crate::Stop;
use crate::error::Error;
use crate::operation::{self, TABLE};

/// The one protocol version this server speaks. A client that asks for another gets
/// this one, and may then disconnect.
const VERSION: &str = "2025-06-18";

const PARSE: (i64, &str) = (-32700, "Parse error");
const REQUEST: (i64, &str) = (-32600, "Invalid Request");
const METHOD: (i64, &str) = (-32601, "Method not found");
const PARAMS: (i64, &str) = (-32602, "Invalid params");

/// The longest line, with its newline, that `serve` reads into memory.
pub(crate) const LIMIT: usize = 1 << 20;

/// Answers each line of `input` with at most one line on `output`, until `input` ends
/// or the reader closes `output`.
pub(crate) fn serve(
    mut input: impl BufRead,
    output: &mut impl Write,
) -> Result<(), Stop> {
    let failed = |e: io::Error| Error::Input {
        message: e.to_string(),
    };
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = (&mut input)
            .take(LIMIT as u64)
            .read_until(b'\n', &mut line)
            .map_err(failed)?;
        let reply = if read == 0 {
            return Ok(());
        } else if read == LIMIT && !line.ends_with(b"\n") {
            input.skip_until(b'\n').map_err(failed)?;
            Some(reply(&Value::Null, Err(fault(REQUEST))).to_string())
        } else {
            // A line that is not UTF-8 is not JSON either, so "" gets the parse error.
            respond(std::str::from_utf8(&line).unwrap_or(""))
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
/// unchanged. A failed operation is a JSON-RPC error whose `data` holds the error's
/// `code`, `message`, and `fix`.
pub(crate) fn respond(message: &str) -> Option<String> {
    let reply = match serde_json::from_str::<Value>(message) {
        Ok(Value::Object(message)) => answer(message)?,
        Ok(_) => reply(&Value::Null, Err(fault(REQUEST))),
        Err(_) => reply(&Value::Null, Err(fault(PARSE))),
    };
    Some(reply.to_string())
}

fn answer(mut message: Map<String, Value>) -> Option<Value> {
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
    let handle: fn(Map<String, Value>) -> Result<Value, Value> = match method.as_str() {
        "initialize" => |_| Ok(initialize()),
        "ping" => |_| Ok(json!({})),
        "tools/list" => |_| Ok(tools()),
        "tools/call" => call,
        _ => return Some(reply(&id, Err(fault(METHOD)))),
    };
    let result = match message.remove("params") {
        None => handle(Map::new()),
        Some(Value::Object(params)) => handle(params),
        Some(_) => Err(fault(PARAMS)),
    };
    Some(reply(&id, result))
}

fn initialize() -> Value {
    json!({
        "protocolVersion": VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "foundation", "version": env!("CARGO_PKG_VERSION") },
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

fn call(mut params: Map<String, Value>) -> Result<Value, Value> {
    let Some(Value::String(name)) = params.remove("name") else {
        return Err(fault(PARAMS));
    };
    let arguments = match params.remove("arguments") {
        None => Map::new(),
        Some(Value::Object(arguments)) => arguments,
        Some(_) => return Err(fault(PARAMS)),
    };
    match operation::read(&name, arguments) {
        Ok(request) => {
            let content = request.run().json();
            Ok(json!({
                "content": [{ "type": "text", "text": content.to_string() }],
                "structuredContent": content,
            }))
        }
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
