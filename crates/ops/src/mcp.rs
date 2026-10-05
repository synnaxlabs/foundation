use std::io::{BufRead, Write};

use serde_json::{Map, Value, json};

use crate::error::Error;
use crate::operation::{self, TABLE};

/// The one protocol version this server speaks. A client that asks for another gets
/// this one, and may then disconnect.
const VERSION: &str = "2025-06-18";

const PARSE: (i64, &str) = (-32700, "Parse error");
const REQUEST: (i64, &str) = (-32600, "Invalid Request");
const METHOD: (i64, &str) = (-32601, "Method not found");
const PARAMS: (i64, &str) = (-32602, "Invalid params");

/// Answers each line of `input` with at most one line on `output`, until `input` ends
/// or the reader closes `output`.
pub(crate) fn serve(
    mut input: impl BufRead,
    output: &mut impl Write,
) -> Result<(), Error> {
    let mut line = Vec::new();
    loop {
        line.clear();
        let read = input
            .read_until(b'\n', &mut line)
            .map_err(|e| Error::Input {
                message: e.to_string(),
            })?;
        if read == 0 {
            return Ok(());
        }
        // A line that is not UTF-8 is not JSON either, so "" gets the parse error.
        let Some(reply) = respond(std::str::from_utf8(&line).unwrap_or("")) else {
            continue;
        };
        if !crate::write(output, &format!("{reply}\n"))? {
            return Ok(());
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
    let params = match message.remove("params") {
        None => Map::new(),
        Some(Value::Object(params)) => params,
        Some(_) => return Some(reply(&id, Err(fault(PARAMS)))),
    };
    let result = match method.as_str() {
        "initialize" => Ok(initialize()),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools()),
        "tools/call" => call(params),
        _ => Err(fault(METHOD)),
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
