//! A2A (Agent2Agent) v1.0, JSON-RPC binding: an HTTP server exposing every agent, and the client the MCP bridge uses.
use crate::acp_client::{AgentEvent, Client};
use crate::config::Config;
use crate::event::{DelegationStep, UiEvent, decider};
use crate::rpc;
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Method, Request, Response, Server};

const A2A_VERSION: &str = "1.0";
const MAX_BODY_BYTES: u64 = 4 << 20;
// Agents can work for a long time (image generation, refactors) before a blocking SendMessage returns.
const CALL_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const SNIPPET_CHARS: usize = 80;
const PARSE_ERROR: i64 = -32700;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const UNSUPPORTED_OPERATION: i64 = -32004;
const STATE_COMPLETED: &str = "TASK_STATE_COMPLETED";
const STATE_FAILED: &str = "TASK_STATE_FAILED";

/// Shared server state, one per process.
struct State {
    config: Arc<Config>,
    ui: Option<Sender<UiEvent>>,
    auto_allow: Arc<AtomicBool>,
    token: String,
    base_url: String,
    next_id: AtomicU64,
}

/// Generates a random bearer token (128 bits, hex) from the OS entropy source.
pub fn new_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| anyhow!("reading OS entropy: {error}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Starts the A2A server on a random loopback port and returns its base URL.
/// Requests are served on background threads for the life of the process.
/// Calls must carry `Authorization: Bearer <token>`; permission prompts go to `ui` unless `auto_allow` is set.
pub fn start(
    config: Arc<Config>,
    ui: Option<Sender<UiEvent>>,
    auto_allow: Arc<AtomicBool>,
    token: String,
) -> Result<String> {
    let server =
        Server::http("127.0.0.1:0").map_err(|error| anyhow!("binding A2A server: {error}"))?;
    let address = server
        .server_addr()
        .to_ip()
        .context("A2A server has no IP address")?;
    let base_url = format!("http://{address}");
    let state = Arc::new(State {
        config,
        ui,
        auto_allow,
        token,
        base_url: base_url.clone(),
        next_id: AtomicU64::new(0),
    });
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let state = state.clone();
            // One thread per request: a SendMessage blocks until the delegated agent finishes.
            std::thread::spawn(move || handle(&state, request));
        }
    });
    Ok(base_url)
}

/// Routes one HTTP request and writes the JSON response.
fn handle(state: &State, mut request: Request) {
    let path = request
        .url()
        .split('?')
        .next()
        .unwrap_or_default()
        .trim_matches('/')
        .to_owned();
    let segments: Vec<&str> = path.split('/').collect();
    let (status, body) = match (request.method(), segments.as_slice()) {
        (Method::Get, ["agents", name, ".well-known", "agent-card.json"]) => {
            match state.config.resolve(name) {
                Ok(_) => (200, agent_card(state, name)),
                Err(error) => (404, json!({"error": format!("{error:#}")})),
            }
        }
        (Method::Post, ["agents", name]) if state.config.resolve(name).is_ok() => {
            if is_authorized(state, &request) {
                (200, rpc_call(state, name, &mut request))
            } else {
                (401, json!({"error": "missing or invalid bearer token"}))
            }
        }
        _ => (404, json!({"error": "not found"})),
    };
    let header =
        Header::from_bytes("Content-Type", "application/json").expect("static header is valid");
    let _ = request.respond(
        Response::from_string(body.to_string())
            .with_status_code(status)
            .with_header(header),
    );
}

/// True when the request carries the server's bearer token.
fn is_authorized(state: &State, request: &Request) -> bool {
    let expected = format!("Bearer {}", state.token);
    request
        .headers()
        .iter()
        .any(|header| header.field.equiv("Authorization") && header.value.as_str() == expected)
}

/// The A2A agent card for `name`, advertising its JSON-RPC endpoint.
fn agent_card(state: &State, name: &str) -> Value {
    json!({
        "name": name,
        "description": format!("{name}, exposed over A2A by central"),
        "supportedInterfaces": [{
            "url": format!("{}/agents/{name}", state.base_url),
            "protocolBinding": "JSONRPC",
            "protocolVersion": A2A_VERSION,
        }],
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": {"streaming": false, "pushNotifications": false},
        "securitySchemes": {"bearer": {"httpAuthSecurityScheme": {"scheme": "Bearer"}}},
        "securityRequirements": [{"schemes": {"bearer": {"list": []}}}],
        "defaultInputModes": ["text/plain"],
        "defaultOutputModes": ["text/plain", "image/png"],
        "skills": [{
            "id": "prompt",
            "name": "Run a prompt",
            "description": "Runs a prompt in the agent's workspace and returns its reply and any generated files.",
            "tags": ["coding", "images"],
        }],
    })
}

/// Parses a JSON-RPC body and dispatches it; only blocking `SendMessage` is implemented.
fn rpc_call(state: &State, agent: &str, request: &mut Request) -> Value {
    let mut body = String::new();
    if request
        .as_reader()
        .take(MAX_BODY_BYTES)
        .read_to_string(&mut body)
        .is_err()
    {
        return rpc::error_response(&Value::Null, PARSE_ERROR, "unreadable request body");
    }
    let Ok(call) = serde_json::from_str::<Value>(&body) else {
        return rpc::error_response(&Value::Null, PARSE_ERROR, "invalid JSON");
    };
    let id = &call["id"];
    match call["method"].as_str() {
        Some("SendMessage") => match TaskRequest::parse(&call["params"]) {
            Ok(task) => rpc::response(id, json!({"task": run_task(state, agent, &task)})),
            Err(error) => rpc::error_response(id, INVALID_PARAMS, &format!("{error:#}")),
        },
        Some("GetTask" | "CancelTask" | "ListTasks") => rpc::error_response(
            id,
            UNSUPPORTED_OPERATION,
            "tasks finish before SendMessage returns",
        ),
        other => rpc::error_response(
            id,
            METHOD_NOT_FOUND,
            &format!("unsupported method {other:?}"),
        ),
    }
}

/// The parts of a SendMessage request this server acts on.
struct TaskRequest {
    prompt: String,
    cwd: PathBuf,
    output_dir: PathBuf,
    caller: String,
    /// Model the caller wants the agent to run instead of its default.
    model: Option<String>,
}

impl TaskRequest {
    /// Extracts the prompt text and the `cwd`/`outputDir`/`caller` metadata from SendMessage params.
    fn parse(params: &Value) -> Result<Self> {
        let message = &params["message"];
        let parts = message["parts"]
            .as_array()
            .context("message.parts is required")?;
        let prompt: Vec<&str> = parts
            .iter()
            .filter_map(|part| part["text"].as_str())
            .collect();
        if prompt.is_empty() {
            bail!("message has no text part");
        }
        let metadata = &message["metadata"];
        let cwd = PathBuf::from(
            metadata["cwd"]
                .as_str()
                .context("message.metadata.cwd is required")?,
        );
        let output_dir = match metadata["outputDir"].as_str() {
            Some(dir) => PathBuf::from(dir),
            None => cwd.join("generated-images"),
        };
        Ok(TaskRequest {
            prompt: prompt.join("\n"),
            cwd,
            output_dir,
            caller: metadata["caller"].as_str().unwrap_or("unknown").to_owned(),
            model: metadata["model"].as_str().map(str::to_owned),
        })
    }
}

/// Runs the request through the agent over ACP and returns a finished A2A Task (completed or failed).
fn run_task(state: &State, agent: &str, request: &TaskRequest) -> Value {
    let task_id = state.new_id("task");
    let context_id = state.new_id("ctx");
    state.log(format!(
        "{} -> {agent}: {}",
        request.caller,
        snippet(&request.prompt)
    ));
    match execute(state, agent, request) {
        Ok(parts) => {
            state.log(format!("{agent} -> {}: completed", request.caller));
            json!({
                "id": task_id,
                "contextId": context_id,
                "status": {"state": STATE_COMPLETED},
                "artifacts": [{"artifactId": state.new_id("artifact"), "name": "reply", "parts": parts}],
            })
        }
        Err(error) => {
            state.log(format!("{agent} -> {}: failed: {error:#}", request.caller));
            let message = json!({
                "messageId": state.new_id("msg"),
                "role": "ROLE_AGENT",
                "parts": [{"text": format!("{error:#}")}],
            });
            json!({
                "id": task_id,
                "contextId": context_id,
                "status": {"state": STATE_FAILED, "message": message},
            })
        }
    }
}

/// Spawns the agent without MCP servers (so delegation cannot loop), runs the prompt,
/// and converts the reply into A2A parts: one text part plus one raw part per generated image.
fn execute(state: &State, agent: &str, request: &TaskRequest) -> Result<Vec<Value>> {
    let exe = std::env::current_exe()?;
    let mut spec = state.config.resolve(agent)?;
    if let Some(model) = &request.model {
        spec = spec.with_model(agent, model)?;
    }
    let launch = spec.launch(agent, &exe, &state.config.path);
    let decide = decider(agent, state.ui.clone(), state.auto_allow.clone());
    let progress_ui = state.ui.clone();
    let progress_agent = agent.to_owned();
    let send_step = {
        let (ui, agent, caller) = (
            progress_ui.clone(),
            progress_agent.clone(),
            request.caller.clone(),
        );
        move |step| {
            if let Some(ui) = &ui {
                let _ = ui.send(UiEvent::Delegation {
                    agent: agent.clone(),
                    caller: caller.clone(),
                    step,
                });
            }
        }
    };
    let report = send_step.clone();
    let on_event = Box::new(move |event| match event {
        AgentEvent::Tool(title) => {
            if let Some(ui) = &progress_ui {
                let _ = ui.send(UiEvent::Activity(format!("{progress_agent} tool: {title}")));
            }
            report(DelegationStep::Tool(title));
        }
        AgentEvent::Text(text) => report(DelegationStep::Text(text)),
        _ => {}
    });
    send_step(DelegationStep::Start(request.prompt.clone()));
    let finish = |result: Result<(), String>| {
        send_step(match result {
            Ok(()) => DelegationStep::Done,
            Err(error) => DelegationStep::Failed(error),
        });
    };
    let outcome = Client::start(
        &launch,
        &request.cwd,
        vec![],
        decide,
        request.output_dir.clone(),
        on_event,
    )
    .and_then(|mut client| client.prompt(&request.prompt));
    let outcome = match outcome {
        Ok(outcome) => {
            finish(Ok(()));
            outcome
        }
        Err(error) => {
            finish(Err(format!("{error:#}")));
            return Err(error);
        }
    };

    let mut parts = vec![json!({"text": outcome.text})];
    for path in &outcome.images {
        let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        parts.push(json!({
            "raw": BASE64.encode(bytes),
            "filename": path.file_name().map(|name| name.to_string_lossy().into_owned()),
            "mediaType": media_type(path),
        }));
    }
    Ok(parts)
}

impl State {
    /// A unique id with the given prefix, unique across the process lifetime.
    fn new_id(&self, prefix: &str) -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        format!(
            "{prefix}-{nanos:x}-{}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    /// Reports a delegation step to the TUI activity pane, or stderr when headless.
    fn log(&self, line: String) {
        match &self.ui {
            Some(ui) => {
                let _ = ui.send(UiEvent::Activity(line));
            }
            None => eprintln!("{line}"),
        }
    }
}

/// First line of `text`, shortened for the activity log.
fn snippet(text: &str) -> String {
    let first_line = text.lines().next().unwrap_or_default();
    match first_line.char_indices().nth(SNIPPET_CHARS) {
        Some((end, _)) => format!("{}...", &first_line[..end]),
        None => first_line.to_owned(),
    }
}

/// MIME type for a generated file, from its extension.
fn media_type(path: &Path) -> &'static str {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "application/octet-stream",
    }
}

/// Where and as whom to call the A2A server.
pub struct Target {
    pub base_url: String,
    pub token: String,
    pub caller: String,
}

/// An agent's answer: its text and the generated files saved locally.
#[derive(Debug)]
pub struct Reply {
    pub text: String,
    pub images: Vec<PathBuf>,
}

/// Sends `text` to `agent` through the A2A server (blocking SendMessage) and saves returned files into `output_dir`.
/// `model` asks the agent to run that model instead of its default.
pub fn send_message(
    target: &Target,
    agent: &str,
    text: &str,
    cwd: &Path,
    output_dir: &Path,
    model: Option<&str>,
) -> Result<Reply> {
    let url = format!("{}/agents/{agent}", target.base_url);
    let message = json!({
        "messageId": format!("msg-{}", std::process::id()),
        "role": "ROLE_USER",
        "parts": [{"text": text}],
        "metadata": {"cwd": cwd, "outputDir": output_dir, "caller": target.caller, "model": model},
    });
    let call = rpc::request(1, "SendMessage", json!({"message": message}));
    let http = ureq::AgentBuilder::new().timeout_read(CALL_TIMEOUT).build();
    let response: Value = http
        .post(&url)
        .set("Authorization", &format!("Bearer {}", target.token))
        .set("A2A-Version", A2A_VERSION)
        .send_json(call)
        .with_context(|| format!("calling {url}"))?
        .into_json()
        .context("A2A response is not JSON")?;
    if let Some(error) = response.get("error") {
        bail!(
            "A2A error from {agent}: {}",
            error["message"].as_str().unwrap_or("unknown")
        );
    }
    parse_task(&response["result"]["task"], output_dir)
}

/// Turns a finished Task into a Reply, saving `raw` parts; a non-completed state becomes an error.
fn parse_task(task: &Value, output_dir: &Path) -> Result<Reply> {
    let state = task["status"]["state"].as_str().unwrap_or("missing");
    if state != STATE_COMPLETED {
        let reason = task["status"]["message"]["parts"][0]["text"]
            .as_str()
            .unwrap_or(state);
        bail!("agent task {state}: {reason}");
    }
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for artifact in task["artifacts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        for part in artifact["parts"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if let Some(text) = part["text"].as_str().filter(|text| !text.is_empty()) {
                texts.push(text.to_owned());
            } else if let Some(raw) = part["raw"].as_str() {
                images.push(save_raw_part(raw, part["filename"].as_str(), output_dir)?);
            }
        }
    }
    Ok(Reply {
        text: texts.join("\n"),
        images,
    })
}

/// Writes a base64 `raw` part into `output_dir`. Only the final component of the
/// remote-supplied filename is used, so a hostile server cannot write outside the directory.
fn save_raw_part(raw: &str, filename: Option<&str>, output_dir: &Path) -> Result<PathBuf> {
    let bytes = BASE64.decode(raw).context("raw part is not valid base64")?;
    let name = filename
        .and_then(|name| Path::new(name).file_name())
        .map_or_else(|| "file.bin".into(), |name| name.to_owned());
    std::fs::create_dir_all(output_dir)?;
    let path = output_dir.join(name);
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!("central-a2a-{}", std::process::id()))
            .join(name)
    }

    // Raw parts are saved under the final path component only, defeating "../" filenames.
    #[test]
    fn raw_part_filename_cannot_escape_output_dir() {
        let dir = test_dir("raw-part");
        let path = save_raw_part("aGk=", Some("../../evil.png"), &dir).unwrap();
        assert_eq!(path, dir.join("evil.png"));
        assert_eq!(std::fs::read(path).unwrap(), b"hi");
    }

    // A completed task yields its text and saved files.
    #[test]
    fn parses_completed_task() {
        let task = json!({"status": {"state": STATE_COMPLETED}, "artifacts": [{"parts": [
            {"text": "done"}, {"raw": "aGk=", "filename": "a.png", "mediaType": "image/png"}]}]});
        let reply = parse_task(&task, &test_dir("parse-task")).unwrap();
        assert_eq!(reply.text, "done");
        assert_eq!(reply.images.len(), 1);
    }

    // A failed task surfaces the agent's message instead of an empty success.
    #[test]
    fn failed_task_is_error() {
        let task =
            json!({"status": {"state": STATE_FAILED, "message": {"parts": [{"text": "boom"}]}}});
        let error = parse_task(&task, &test_dir("failed")).unwrap_err();
        assert!(error.to_string().contains("boom"));
    }

    // Requests without a cwd are rejected because agents need an explicit workspace.
    #[test]
    fn task_request_requires_cwd_and_text() {
        let ok = json!({"message": {"parts": [{"text": "hi"}], "metadata": {"cwd": "/w"}}});
        assert!(TaskRequest::parse(&ok).is_ok());
        let with_model = json!({"message": {"parts": [{"text": "hi"}], "metadata": {"cwd": "/w", "model": "m"}}});
        assert_eq!(
            TaskRequest::parse(&with_model).unwrap().model.as_deref(),
            Some("m")
        );
        assert!(TaskRequest::parse(&json!({"message": {"parts": [{"text": "hi"}]}})).is_err());
        assert!(
            TaskRequest::parse(&json!({"message": {"parts": [], "metadata": {"cwd": "/w"}}}))
                .is_err()
        );
    }

    // Long prompts are cut on a char boundary, never mid-codepoint.
    #[test]
    fn snippet_truncates_on_char_boundary() {
        let long = "é".repeat(200);
        assert_eq!(snippet(&long).chars().count(), SNIPPET_CHARS + 3);
        assert_eq!(snippet("a\nb"), "a");
    }
}
