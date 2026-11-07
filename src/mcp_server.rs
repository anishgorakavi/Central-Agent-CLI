//! MCP stdio server exposing `delegate` and `generate_image` tools; each call is forwarded to another agent over A2A.
use crate::a2a::{self, Target};
use crate::rpc::{self, read_message, write_message};
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const METHOD_NOT_FOUND: i64 = -32601;
const IMAGE_DIR_NAME: &str = "generated-images";
/// Environment variable carrying the A2A bearer token, so it never appears in a process listing.
pub const TOKEN_ENV: &str = "CENTRAL_A2A_TOKEN";

/// The ACP `mcpServers` entry that makes `caller` launch this binary as its delegation toolbox.
pub fn session_entry(
    exe: &Path,
    config_path: &Path,
    a2a_url: &str,
    token: &str,
    caller: &str,
    image_agent: &str,
) -> Value {
    json!({
        "name": "central",
        "command": exe,
        "args": ["--config", config_path, "mcp", "--a2a-url", a2a_url, "--caller", caller, "--image-agent", image_agent],
        "env": [{"name": TOKEN_ENV, "value": token}],
    })
}

/// Serves MCP on stdin/stdout until the client closes the pipe.
/// `image_agent` is the agent that `generate_image` delegates to.
pub fn serve(target: &Target, image_agent: &str) -> Result<()> {
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    while let Some(message) = read_message(&mut stdin)? {
        // Notifications such as notifications/initialized need no reply.
        let Some(id) = message.get("id") else {
            continue;
        };
        let params = &message["params"];
        let reply = match message["method"].as_str().unwrap_or_default() {
            "initialize" => rpc::response(
                id,
                json!({
                    "protocolVersion": params["protocolVersion"].as_str().unwrap_or(MCP_PROTOCOL_VERSION),
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "central", "version": env!("CARGO_PKG_VERSION")},
                }),
            ),
            "ping" => rpc::response(id, json!({})),
            "tools/list" => rpc::response(id, json!({"tools": tool_definitions()})),
            "tools/call" => rpc::response(id, call_tool(target, image_agent, params)),
            other => {
                rpc::error_response(id, METHOD_NOT_FOUND, &format!("unsupported method {other}"))
            }
        };
        write_message(&mut stdout, &reply)?;
    }
    Ok(())
}

/// JSON schemas advertised through `tools/list`.
fn tool_definitions() -> Value {
    json!([
        {
            "name": "generate_image",
            "description": "Generate an image from a text description by delegating to an image-capable agent (Codex) over A2A. Returns the saved file path(s).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "What the image should show."},
                    "output_dir": {"type": "string", "description": "Directory to save into; defaults to ./generated-images."},
                },
                "required": ["prompt"],
            },
        },
        {
            "name": "delegate",
            "description": "Run a prompt in another coding agent (claude, codex, antigravity, or a custom agent) over A2A and return its reply.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "agent": {"type": "string", "description": "Agent name, see `central agents`."},
                    "prompt": {"type": "string"},
                    "cwd": {"type": "string", "description": "Working directory for the agent; defaults to the current one."},
                },
                "required": ["agent", "prompt"],
            },
        },
    ])
}

/// Runs a tool and wraps its outcome in an MCP result; failures become `isError` results, not protocol errors.
fn call_tool(target: &Target, image_agent: &str, params: &Value) -> Value {
    let args = &params["arguments"];
    let outcome = match params["name"].as_str() {
        Some("generate_image") => generate_image(target, image_agent, args),
        Some("delegate") => delegate(target, args),
        other => Err(anyhow!("unknown tool {other:?}")),
    };
    let (text, is_error) = match outcome {
        Ok(text) => (text, false),
        Err(error) => (format!("{error:#}"), true),
    };
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

/// Handles `generate_image`: asks the image agent to render and save the picture.
fn generate_image(target: &Target, image_agent: &str, args: &Value) -> Result<String> {
    let description = required_str(args, "prompt")?;
    let cwd = std::env::current_dir()?;
    let image_dir = match args["output_dir"].as_str() {
        Some(dir) => cwd.join(dir),
        None => cwd.join(IMAGE_DIR_NAME),
    };
    let prompt = format!(
        "Generate an image with your built-in image generation tool. Description: {description}\n\
         Save the resulting file inside {} and reply with its absolute path.",
        image_dir.display()
    );
    forward(target, image_agent, &prompt, &cwd, &image_dir)
}

/// Handles `delegate`: forwards a prompt to any named agent.
fn delegate(target: &Target, args: &Value) -> Result<String> {
    let cwd = match args["cwd"].as_str() {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir()?,
    };
    let image_dir = cwd.join(IMAGE_DIR_NAME);
    forward(
        target,
        required_str(args, "agent")?,
        required_str(args, "prompt")?,
        &cwd,
        &image_dir,
    )
}

/// Sends the prompt over A2A and formats the reply plus saved image paths for the calling model.
fn forward(
    target: &Target,
    agent: &str,
    prompt: &str,
    cwd: &Path,
    image_dir: &Path,
) -> Result<String> {
    let reply = a2a::send_message(target, agent, prompt, cwd, image_dir)?;
    let mut report = reply.text;
    for path in &reply.images {
        report.push_str(&format!("\nSaved image: {}", path.display()));
    }
    Ok(report)
}

/// Returns a required string argument or a descriptive error.
fn required_str<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args[name]
        .as_str()
        .with_context(|| format!("missing string argument '{name}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A target pointing at an unreachable server, for tests that never reach it.
    fn target() -> Target {
        Target {
            base_url: "http://127.0.0.1:1".into(),
            token: "t".into(),
            caller: "claude".into(),
        }
    }

    // Claude discovers tools from this list, so both names must stay stable.
    #[test]
    fn lists_both_tools() {
        let definitions = tool_definitions();
        let names: Vec<&str> = definitions
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["generate_image", "delegate"]);
    }

    // Tool failures must reach the model as isError results so it can react.
    #[test]
    fn unknown_tool_is_error_result() {
        let result = call_tool(&target(), "codex", &json!({"name": "nope"}));
        assert_eq!(result["isError"], true);
    }

    // The token travels in the environment, never in argv.
    #[test]
    fn session_entry_keeps_token_out_of_args() {
        let entry = session_entry(
            Path::new("/bin/central"),
            Path::new("/c.json"),
            "http://x",
            "secret",
            "claude",
            "codex",
        );
        assert!(!entry["args"].to_string().contains("secret"));
        assert_eq!(entry["env"][0]["value"], "secret");
    }
}
