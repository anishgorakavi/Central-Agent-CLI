//! Custom agent harness: an ACP agent (stdio server) that answers prompts with an Ollama or OpenAI-compatible model.
use crate::config::AgentSpec;
use crate::llm;
use crate::rpc::{self, read_message, write_message};
use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;

const ACP_PROTOCOL_VERSION: u32 = 1;
const METHOD_NOT_FOUND: i64 = -32601;
const INTERNAL_ERROR: i64 = -32603;

/// Serves ACP on stdin/stdout until the client closes the pipe. Each session keeps its own chat history.
/// A prompt blocks the loop, so `session/cancel` is not honored mid-stream.
pub fn serve(spec: &AgentSpec) -> Result<()> {
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut sessions: HashMap<String, Vec<Value>> = HashMap::new();

    while let Some(message) = read_message(&mut stdin)? {
        // Notifications (e.g. session/cancel) carry no id and need no reply.
        let Some(id) = message.get("id") else {
            continue;
        };
        let reply = match message["method"].as_str().unwrap_or_default() {
            "initialize" => rpc::response(
                id,
                json!({
                    "protocolVersion": ACP_PROTOCOL_VERSION,
                    "agentCapabilities": {"promptCapabilities": {}},
                    "authMethods": [],
                }),
            ),
            "session/new" => {
                let session_id = format!("session-{}", sessions.len() + 1);
                sessions.insert(session_id.clone(), Vec::new());
                rpc::response(id, json!({"sessionId": session_id}))
            }
            "session/prompt" => {
                match run_prompt(spec, &mut sessions, &message["params"], &mut stdout) {
                    Ok(()) => rpc::response(id, json!({"stopReason": "end_turn"})),
                    Err(error) => rpc::error_response(id, INTERNAL_ERROR, &format!("{error:#}")),
                }
            }
            other => {
                rpc::error_response(id, METHOD_NOT_FOUND, &format!("unsupported method {other}"))
            }
        };
        write_message(&mut stdout, &reply)?;
    }
    Ok(())
}

/// Appends the prompt to the session history, streams the model's reply as `agent_message_chunk` updates,
/// and records the full reply. A failed call leaves the history as it was before the prompt.
fn run_prompt(
    spec: &AgentSpec,
    sessions: &mut HashMap<String, Vec<Value>>,
    params: &Value,
    out: &mut impl Write,
) -> Result<()> {
    let session_id = params["sessionId"].as_str().context("missing sessionId")?;
    let history = sessions
        .get_mut(session_id)
        .with_context(|| format!("unknown session {session_id}"))?;
    history.push(json!({"role": "user", "content": prompt_text(&params["prompt"])}));

    let mut reply = String::new();
    let streamed = llm::stream_chat(spec, history, &mut |chunk| {
        reply.push_str(chunk);
        let update = json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": chunk}});
        write_message(
            out,
            &rpc::notification(
                "session/update",
                json!({"sessionId": session_id, "update": update}),
            ),
        )
    });
    if let Err(error) = streamed {
        history.pop();
        return Err(error);
    }
    history.push(json!({"role": "assistant", "content": reply}));
    Ok(())
}

/// Flattens prompt content blocks to plain text; non-text blocks are not supported by this harness.
fn prompt_text(blocks: &Value) -> String {
    let parts: Vec<&str> = blocks
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|block| match block["type"].as_str() {
            Some("text") => block["text"].as_str(),
            Some("resource") => block["resource"]["text"].as_str(),
            _ => None,
        })
        .collect();
    parts.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Embedded resources are inlined; unsupported blocks (images) are dropped.
    #[test]
    fn flattens_text_and_resources() {
        let blocks = json!([
            {"type": "text", "text": "look"},
            {"type": "resource", "resource": {"uri": "f", "text": "contents"}},
            {"type": "image", "data": "x", "mimeType": "image/png"},
        ]);
        assert_eq!(prompt_text(&blocks), "look\ncontents");
    }
}
