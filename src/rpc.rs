//! Newline-delimited JSON-RPC 2.0 framing shared by the ACP client, the custom ACP agent and the MCP server.
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::io::{BufRead, Write};

/// Reads the next JSON message; returns None at end of input.
/// Lines that are not JSON are skipped silently: agents sometimes print log noise on stdout,
/// and writing it to stderr would corrupt the TUI screen.
pub fn read_message(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).context("reading message")? == 0 {
            return Ok(None);
        }
        if let Ok(message) = serde_json::from_str(line.trim()) {
            return Ok(Some(message));
        }
    }
}

/// Writes one message followed by a newline and flushes so the peer sees it immediately.
pub fn write_message(writer: &mut impl Write, message: &Value) -> Result<()> {
    writeln!(writer, "{message}").context("writing message")?;
    writer.flush().context("flushing message")
}

/// Builds a JSON-RPC request.
pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// Builds a JSON-RPC notification (no id, no reply expected).
pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// Builds a successful JSON-RPC response for the request with the given id.
pub fn response(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// Builds a JSON-RPC error response for the request with the given id.
pub fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;

    // Garbage and blank lines must not stop the reader from reaching real messages.
    #[test]
    fn skips_non_json_lines() {
        let mut input = "starting up\n\n{\"id\":1}\n".as_bytes();
        let message = read_message(&mut input).unwrap().unwrap();
        assert_eq!(message["id"], 1);
        assert!(read_message(&mut input).unwrap().is_none());
    }

    // Written messages are single lines so the peer can split on newlines.
    #[test]
    fn writes_one_line_per_message() {
        let mut out = Vec::new();
        write_message(&mut out, &notification("x", json!({}))).unwrap();
        assert_eq!(String::from_utf8(out).unwrap().matches('\n').count(), 1);
    }
}
