//! banto's MCP server (spike stage).
//!
//! This is the brigade Director<->Worker mediation channel, exposed to an
//! embedded `claude` process over the Model Context Protocol. An embedded
//! session is launched with `claude --mcp-config <file>` pointing at
//! `banto _mcp --session <id>`; Claude Code spawns that as a stdio MCP server
//! and speaks JSON-RPC 2.0 to it. Because banto controls the launch argv, the
//! config file lives under banto's own data dir and nothing is ever written
//! under `~/.claude` (read-only invariant 1).
//!
//! Spike scope: prove the transport end to end — the `initialize` handshake,
//! `tools/list`, and a `tools/call` round-trip — with a single trivial
//! `banto_ping` tool that echoes the calling session id. The real message
//! tools (`send_to_worker` / `check_messages`, backed by a store-side queue)
//! layer on once this is confirmed against real Claude Code.
//!
//! Transport: MCP stdio — newline-delimited JSON-RPC 2.0 messages on
//! stdin/stdout (no Content-Length framing). Requests carry an `id` and get a
//! response; notifications (no `id`, e.g. `notifications/initialized`) get
//! none. Anything not a valid MCP message must stay off stdout, so diagnostics
//! would go to stderr.

use std::io::{self, BufRead, Write};

use anyhow::Result;
use serde_json::{Value, json};

/// Per-connection context: who banto launched this server for.
struct ServerContext {
    /// The calling session's id (echoed by `banto_ping`; the brigade role/id
    /// will join it as the message tools land).
    session: Option<String>,
}

/// Run the MCP server on stdio until the client closes the connection (EOF on
/// stdin). Spawned by an embedded `claude` via `--mcp-config`.
pub fn run_stdio_server(session: Option<String>) -> Result<()> {
    let ctx = ServerContext { session };
    let stdin = io::stdin();
    let mut reader = stdin.lock();
    let stdout = io::stdout();
    let mut writer = stdout.lock();

    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break; // EOF: the client closed the connection.
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(response) = handle_line(trimmed, &ctx) {
            writer.write_all(response.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
    }
    Ok(())
}

/// Handle one JSON-RPC message line, returning the response line to write back
/// (or `None` for notifications and unparseable input, which get no reply).
fn handle_line(line: &str, ctx: &ServerContext) -> Option<String> {
    let msg: Value = serde_json::from_str(line).ok()?;
    let method = msg.get("method").and_then(Value::as_str)?;

    // Notifications carry no id and are never answered.
    let id = match msg.get("id") {
        Some(id) if !id.is_null() => id.clone(),
        _ => return None,
    };

    let result = match method {
        "initialize" => initialize_result(&msg),
        "tools/list" => tools_list_result(),
        "tools/call" => tools_call_result(&msg, ctx),
        "ping" => json!({}),
        other => {
            return Some(error_response(
                id,
                -32601,
                &format!("method not found: {other}"),
            ));
        }
    };
    Some(success_response(id, result))
}

/// `initialize`: advertise tool support and echo the client's requested
/// protocol version (falling back to a known one) so version negotiation just
/// works during the spike.
fn initialize_result(msg: &Value) -> Value {
    let version = msg
        .pointer("/params/protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or("2024-11-05");
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "banto", "version": env!("CARGO_PKG_VERSION") },
    })
}

/// `tools/list`: the single spike tool.
fn tools_list_result() -> Value {
    json!({
        "tools": [ {
            "name": "banto_ping",
            "description": "Health check: returns a pong from banto, echoing the calling \
                            session id. A spike stand-in for the brigade Director<->Worker \
                            message tools.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false,
            },
        } ],
    })
}

/// `tools/call`: dispatch the requested tool.
fn tools_call_result(msg: &Value, ctx: &ServerContext) -> Value {
    let name = msg
        .pointer("/params/name")
        .and_then(Value::as_str)
        .unwrap_or("");
    match name {
        "banto_ping" => {
            let text = match &ctx.session {
                Some(session) => format!("pong from banto (session={session})"),
                None => "pong from banto".to_string(),
            };
            json!({ "content": [ { "type": "text", "text": text } ], "isError": false })
        }
        other => json!({
            "content": [ { "type": "text", "text": format!("unknown tool: {other}") } ],
            "isError": true,
        }),
    }
}

fn success_response(id: Value, result: Value) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()
}

fn error_response(id: Value, code: i64, message: &str) -> String {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> ServerContext {
        ServerContext {
            session: Some("spike-session".to_string()),
        }
    }

    fn call(line: &str) -> Value {
        let response = handle_line(line, &ctx()).expect("expected a response");
        serde_json::from_str(&response).unwrap()
    }

    #[test]
    fn initialize_echoes_protocol_version_and_advertises_tools() {
        let response = call(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion":"2025-06-18","capabilities":{}}}"#,
        );
        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
        assert_eq!(response["result"]["serverInfo"]["name"], "banto");
        assert!(response["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn tools_list_includes_banto_ping() {
        let response = call(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#);
        let tools = response["result"]["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "banto_ping");
        assert!(tools[0]["inputSchema"].is_object());
    }

    #[test]
    fn tools_call_ping_echoes_the_session() {
        let response = call(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call",
                "params":{"name":"banto_ping","arguments":{}}}"#,
        );
        let text = response["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("pong"), "got {text:?}");
        assert!(text.contains("spike-session"), "got {text:?}");
        assert_eq!(response["result"]["isError"], false);
    }

    #[test]
    fn tools_call_unknown_tool_is_an_error_result() {
        let response = call(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call",
                "params":{"name":"nope","arguments":{}}}"#,
        );
        assert_eq!(response["result"]["isError"], true);
    }

    #[test]
    fn notifications_get_no_response() {
        assert!(
            handle_line(
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                &ctx()
            )
            .is_none()
        );
    }

    #[test]
    fn unknown_method_returns_method_not_found() {
        let response = call(r#"{"jsonrpc":"2.0","id":5,"method":"resources/list"}"#);
        assert_eq!(response["error"]["code"], -32601);
    }

    #[test]
    fn garbage_input_is_ignored() {
        assert!(handle_line("not json", &ctx()).is_none());
    }
}
