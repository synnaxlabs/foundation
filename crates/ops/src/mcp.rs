use serde_json::{Map, Value, json};

/// The one protocol version this server speaks. A client that asks for another gets
/// this one, and may then disconnect.
const VERSION: &str = "2025-06-18";

const PARSE: (i64, &str) = (-32700, "Parse error");
const REQUEST: (i64, &str) = (-32600, "Invalid Request");
const METHOD: (i64, &str) = (-32601, "Method not found");
const PARAMS: (i64, &str) = (-32602, "Invalid params");

/// Answers one MCP message, a JSON-RPC 2.0 request or notification as one line of
/// JSON. Returns the reply as one line of JSON, or `None` for a notification. Does no
/// I/O: the caller reads each line from the client and writes each reply.
#[must_use]
pub fn mcp(message: &str) -> Option<String> {
    let reply = match serde_json::from_str::<Value>(message) {
        Ok(Value::Object(message)) => answer_object(&message)?,
        Ok(_) => error(&Value::Null, REQUEST),
        Err(_) => error(&Value::Null, PARSE),
    };
    Some(reply.to_string())
}

fn answer_object(message: &Map<String, Value>) -> Option<Value> {
    // A message with no `id` is a notification, which never gets a reply.
    let id = message.get("id")?;
    if !(id.is_string() || id.is_number())
        || message.get("jsonrpc") != Some(&json!("2.0"))
    {
        return Some(error(&Value::Null, REQUEST));
    }
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return Some(error(id, REQUEST));
    };
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(initialize()),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(crate::tools()),
        "tools/call" => call(params),
        _ => Err(METHOD),
    };
    Some(match result {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(code) => error(id, code),
    })
}

fn initialize() -> Value {
    json!({
        "protocolVersion": VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "foundation", "version": env!("CARGO_PKG_VERSION") },
    })
}

fn call(params: Value) -> Result<Value, (i64, &'static str)> {
    let Value::Object(mut params) = params else {
        return Err(PARAMS);
    };
    let Some(Value::String(name)) = params.remove("name") else {
        return Err(PARAMS);
    };
    let arguments = params.remove("arguments").unwrap_or(Value::Null);
    Ok(crate::call(&name, arguments))
}

fn error(id: &Value, (code, message): (i64, &str)) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
