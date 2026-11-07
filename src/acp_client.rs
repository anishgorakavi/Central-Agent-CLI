//! ACP client: spawns an agent process, performs the handshake, runs prompts, and reports text, tool use and images.
use crate::config::Launch;
use crate::rpc::{self, read_message, write_message};
use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::{Value, json};
use std::fmt;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const ACP_PROTOCOL_VERSION: u32 = 1;
const METHOD_NOT_FOUND: i64 = -32601;
const AUTH_REQUIRED: i64 = -32000;

/// Progress reported while an agent works. The client emits Text, UserText, Tool and Image
/// (UserText only while replaying a loaded session); the TUI worker adds the rest.
pub enum AgentEvent {
    Started,
    Text(String),
    UserText(String),
    Tool(String),
    Image(PathBuf),
    Sessions(Vec<SessionInfo>),
    /// A past session was loaded and its history replayed.
    Resumed,
    Done,
    /// A non-fatal problem; the agent process is still usable.
    Notice(String),
    /// The agent is unusable (failed to start or died).
    Failed(String),
}

/// A past conversation an agent can reopen.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionInfo {
    pub id: String,
    pub title: Option<String>,
    pub updated_at: Option<String>,
}

/// A JSON-RPC error reply from the agent, kept structured so callers can react to its code.
#[derive(Debug)]
struct RpcError {
    method: String,
    code: i64,
    message: String,
}

impl fmt::Display for RpcError {
    /// Formats as `<method> failed: <message>`.
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{} failed: {}", self.method, self.message)
    }
}

impl std::error::Error for RpcError {}

/// Answers a permission request: receives the tool title and the agent's options,
/// returns the chosen option id, or None to cancel.
pub type Decide = Box<dyn FnMut(&str, &[Value]) -> Option<String> + Send>;

/// What a finished prompt turn produced.
pub struct PromptOutcome {
    pub text: String,
    pub images: Vec<PathBuf>,
}

/// A live ACP session with one spawned agent. The agent process is killed on drop.
pub struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    session_id: String,
    cwd: PathBuf,
    capabilities: Value,
    /// Ids of the login methods the agent advertised, in preference order.
    auth_methods: Vec<String>,
    /// True while a loaded session's history is being replayed, so old images are not re-saved.
    replaying: bool,
    decide: Decide,
    image_dir: PathBuf,
    on_event: Box<dyn FnMut(AgentEvent) + Send>,
    text: String,
    images: Vec<PathBuf>,
}

impl Client {
    /// Spawns the agent in `cwd` and opens a fresh session.
    /// `mcp_servers` are ACP-format MCP server entries the agent should connect to.
    /// Images the agent returns are saved under `image_dir`; progress goes to `on_event`.
    pub fn start(
        launch: &Launch,
        cwd: &Path,
        mcp_servers: Vec<Value>,
        decide: Decide,
        image_dir: PathBuf,
        on_event: Box<dyn FnMut(AgentEvent) + Send>,
    ) -> Result<Self> {
        let mut client = Self::connect(launch, cwd, decide, image_dir, on_event)?;
        client.open_new(mcp_servers)?;
        Ok(client)
    }

    /// Spawns the agent in `cwd` and runs `initialize`, without opening a session yet,
    /// so the caller can list or load past sessions first.
    pub fn connect(
        launch: &Launch,
        cwd: &Path,
        decide: Decide,
        image_dir: PathBuf,
        on_event: Box<dyn FnMut(AgentEvent) + Send>,
    ) -> Result<Self> {
        let mut child = Command::new(&launch.program)
            .args(&launch.args)
            .envs(&launch.env)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Agents log to stderr; inheriting it would corrupt the TUI screen.
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("starting {}", launch.program))?;
        let stdin = child.stdin.take().context("agent stdin unavailable")?;
        let stdout = BufReader::new(child.stdout.take().context("agent stdout unavailable")?);
        let mut client = Client {
            child,
            stdin,
            stdout,
            next_id: 0,
            session_id: String::new(),
            cwd: cwd.to_path_buf(),
            capabilities: Value::Null,
            auth_methods: Vec::new(),
            replaying: false,
            decide,
            image_dir,
            on_event,
            text: String::new(),
            images: Vec::new(),
        };

        // No fs or terminal capabilities are offered: agents use their own file access,
        // so this client never touches the disk on an agent's behalf.
        let init = client.call(
            "initialize",
            json!({
                "protocolVersion": ACP_PROTOCOL_VERSION,
                "clientCapabilities": {},
                "clientInfo": {"name": "central", "version": env!("CARGO_PKG_VERSION")},
            }),
        )?;
        client.capabilities = init["agentCapabilities"].clone();
        client.auth_methods = init["authMethods"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|method| method["id"].as_str().map(str::to_owned))
            .collect();
        Ok(client)
    }

    /// True once a session is open and prompts can be sent.
    pub fn has_session(&self) -> bool {
        !self.session_id.is_empty()
    }

    /// Opens a fresh session, replacing any current one.
    pub fn open_new(&mut self, mcp_servers: Vec<Value>) -> Result<()> {
        let created = self.call_authenticated(
            "session/new",
            json!({"cwd": self.cwd, "mcpServers": mcp_servers}),
        )?;
        self.session_id = created["sessionId"]
            .as_str()
            .context("session/new returned no sessionId")?
            .to_owned();
        Ok(())
    }

    /// Lists the agent's past sessions for this directory, newest first when the agent reports timestamps.
    pub fn list_sessions(&mut self) -> Result<Vec<SessionInfo>> {
        if self.capabilities["sessionCapabilities"]["list"].is_null() {
            bail!("this agent does not support listing sessions");
        }
        let result = self.call_authenticated("session/list", json!({"cwd": self.cwd}))?;
        let mut sessions: Vec<SessionInfo> = result["sessions"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|session| {
                Some(SessionInfo {
                    id: session["sessionId"].as_str()?.to_owned(),
                    title: session["title"].as_str().map(str::to_owned),
                    updated_at: session["updatedAt"].as_str().map(str::to_owned),
                })
            })
            .collect();
        // ISO 8601 timestamps sort lexicographically; sessions without one keep their order at the end.
        sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(sessions)
    }

    /// Reopens a past session. Prefers `session/load`, which replays the history as events;
    /// falls back to `session/resume`, which restores context without replaying it.
    pub fn open_existing(&mut self, id: &str, mcp_servers: Vec<Value>) -> Result<()> {
        let params = json!({"sessionId": id, "cwd": self.cwd, "mcpServers": mcp_servers});
        if self.capabilities["loadSession"] == true {
            self.replaying = true;
            let loaded = self.call_authenticated("session/load", params);
            self.replaying = false;
            loaded?;
        } else if !self.capabilities["sessionCapabilities"]["resume"].is_null() {
            self.call_authenticated("session/resume", params)?;
        } else {
            bail!("this agent cannot reopen past sessions");
        }
        self.session_id = id.to_owned();
        // Replayed text is history, not a reply to any prompt.
        self.text.clear();
        self.images.clear();
        Ok(())
    }

    /// Sends one user prompt and blocks until the agent finishes its turn.
    pub fn prompt(&mut self, text: &str) -> Result<PromptOutcome> {
        self.text.clear();
        self.images.clear();
        self.call(
            "session/prompt",
            json!({"sessionId": self.session_id, "prompt": [{"type": "text", "text": text}]}),
        )?;
        Ok(PromptOutcome {
            text: std::mem::take(&mut self.text),
            images: std::mem::take(&mut self.images),
        })
    }

    /// Sends a request and serves the agent's own requests and notifications until the reply arrives.
    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        write_message(&mut self.stdin, &rpc::request(id, method, params))?;
        loop {
            let message = read_message(&mut self.stdout)?
                .with_context(|| format!("agent exited before replying to {method}"))?;
            let name = message.get("method").and_then(Value::as_str);
            match (message.get("id"), name) {
                (Some(request_id), Some(name)) => {
                    let reply = match self.answer_agent_request(name, &message["params"]) {
                        Some(result) => rpc::response(request_id, result),
                        None => rpc::error_response(
                            request_id,
                            METHOD_NOT_FOUND,
                            &format!("unsupported method {name}"),
                        ),
                    };
                    write_message(&mut self.stdin, &reply)?;
                }
                (None, Some("session/update")) => self.apply_update(&message["params"]["update"]),
                (Some(reply_id), None) if *reply_id == json!(id) => {
                    if let Some(error) = message.get("error") {
                        return Err(RpcError {
                            method: method.to_owned(),
                            code: error["code"].as_i64().unwrap_or_default(),
                            message: error["message"]
                                .as_str()
                                .unwrap_or("unknown error")
                                .to_owned(),
                        }
                        .into());
                    }
                    return Ok(message["result"].clone());
                }
                _ => {}
            }
        }
    }

    /// Like `call`, but if the agent says authentication is required, logs in with its first advertised
    /// method and retries once. This reuses credentials the agent's own CLI already stored; no secret
    /// is passed from here.
    fn call_authenticated(&mut self, method: &str, params: Value) -> Result<Value> {
        match self.call(method, params.clone()) {
            Err(error) if is_auth_required(&error) => {
                let method_id = self.auth_methods.first().cloned().ok_or(error)?;
                self.call("authenticate", json!({"methodId": method_id}))?;
                self.call(method, params)
            }
            other => other,
        }
    }

    /// Handles a request sent by the agent; None means the method is not supported.
    fn answer_agent_request(&mut self, method: &str, params: &Value) -> Option<Value> {
        if method != "session/request_permission" {
            return None;
        }
        let options = params["options"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        let title = params["toolCall"]["title"].as_str().unwrap_or("tool call");
        let outcome = match (self.decide)(title, options) {
            Some(option_id) => json!({"outcome": "selected", "optionId": option_id}),
            None => json!({"outcome": "cancelled"}),
        };
        Some(json!({"outcome": outcome}))
    }

    /// Records message text and images from a `session/update` notification.
    fn apply_update(&mut self, update: &Value) {
        match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => self.collect_block(&update["content"], true),
            Some("user_message_chunk") => {
                if let Some(text) = update["content"]["text"].as_str() {
                    (self.on_event)(AgentEvent::UserText(text.to_owned()));
                }
            }
            Some("tool_call") => {
                let title = update["title"].as_str().unwrap_or("tool call");
                (self.on_event)(AgentEvent::Tool(title.to_owned()));
            }
            Some("tool_call_update") => {
                for item in update["content"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                {
                    if item["type"] == "content" {
                        self.collect_block(&item["content"], false);
                    }
                }
            }
            _ => {}
        }
    }

    /// Keeps one content block: text only when `keep_text` (tool output text is noise), images always.
    fn collect_block(&mut self, block: &Value, keep_text: bool) {
        match block["type"].as_str() {
            Some("text") if keep_text => {
                let text = block["text"].as_str().unwrap_or_default();
                self.text.push_str(text);
                (self.on_event)(AgentEvent::Text(text.to_owned()));
            }
            Some("image") if self.replaying => {}
            Some("image") => {
                let data = block["data"].as_str().unwrap_or_default();
                let mime = block["mimeType"].as_str().unwrap_or_default();
                match save_image(&self.image_dir, data, mime, self.images.len()) {
                    Ok(path) => {
                        self.images.push(path.clone());
                        (self.on_event)(AgentEvent::Image(path));
                    }
                    Err(error) => (self.on_event)(AgentEvent::Tool(format!(
                        "could not save image: {error:#}"
                    ))),
                }
            }
            _ => {}
        }
    }
}

impl Drop for Client {
    /// Stops the agent so no orphan process outlives the session.
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// True when `error` is the agent's "authentication required" reply.
fn is_auth_required(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<RpcError>()
        .is_some_and(|rpc| rpc.code == AUTH_REQUIRED)
}

/// Picks the best allow (or reject) option id; once-variants win over always-variants.
pub fn pick_option(options: &[Value], allow: bool) -> Option<String> {
    let kinds = if allow {
        ["allow_once", "allow_always"]
    } else {
        ["reject_once", "reject_always"]
    };
    kinds.iter().find_map(|kind| {
        options
            .iter()
            .find(|option| option["kind"] == *kind)
            .and_then(|option| option["optionId"].as_str())
            .map(str::to_owned)
    })
}

/// Decision policy that declines every permission request.
pub fn reject_all() -> Decide {
    Box::new(|_, options| pick_option(options, false))
}

/// Decodes a base64 image and writes it to `dir` under a unique name; returns the file path.
fn save_image(dir: &Path, data: &str, mime: &str, index: usize) -> Result<PathBuf> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .context("image data is not valid base64")?;
    let extension = match mime {
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        "image/gif" => "gif",
        _ => "bin",
    };
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("image-{nanos}-{index}.{extension}"));
    std::fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Three permission options: an always-allow, a once-allow and a once-reject.
    fn options() -> Vec<Value> {
        vec![
            json!({"optionId": "a", "name": "Always", "kind": "allow_always"}),
            json!({"optionId": "o", "name": "Once", "kind": "allow_once"}),
            json!({"optionId": "r", "name": "No", "kind": "reject_once"}),
        ]
    }

    // Auto-approval should grant the narrowest permission available.
    #[test]
    fn picks_once_before_always() {
        assert_eq!(pick_option(&options(), true).as_deref(), Some("o"));
        assert_eq!(pick_option(&options(), false).as_deref(), Some("r"));
    }

    // With no matching option the client must cancel rather than guess.
    #[test]
    fn no_matching_option_cancels() {
        assert_eq!(pick_option(&options()[2..], true), None);
    }

    // Images land on disk with an extension derived from the MIME type.
    #[test]
    fn saves_png_image() {
        let dir = std::env::temp_dir().join(format!("central-test-{}", std::process::id()));
        let path = save_image(&dir, "aGVsbG8=", "image/png", 0).unwrap();
        assert_eq!(path.extension().unwrap(), "png");
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Launches tests/fixtures/fake_agent.py and returns the client plus the events it emits.
    fn fake_agent() -> (Client, std::sync::mpsc::Receiver<String>) {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_agent.py");
        let launch = Launch {
            program: "python3".to_owned(),
            args: vec!["-I".to_owned(), script.to_string_lossy().into_owned()],
            env: Default::default(),
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let on_event = Box::new(move |event: AgentEvent| {
            let label = match event {
                AgentEvent::Text(t) => format!("text:{t}"),
                AgentEvent::UserText(t) => format!("user:{t}"),
                AgentEvent::Image(_) => "image".to_owned(),
                _ => return,
            };
            let _ = tx.send(label);
        });
        let dir = std::env::temp_dir().join(format!("central-fake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        (
            Client::connect(&launch, &dir, reject_all(), dir.join("img"), on_event).unwrap(),
            rx,
        )
    }

    // Sessions come back newest first, and the missing login is handled transparently.
    #[test]
    fn lists_sessions_newest_first_after_auto_login() {
        let (mut client, _events) = fake_agent();
        let sessions = client.list_sessions().unwrap();
        let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["new", "old"]);
        assert_eq!(sessions[0].title.as_deref(), Some("newer chat"));
    }

    // Loading replays history as events (without saving old images) and later prompts use the loaded session.
    #[test]
    fn load_replays_history_then_continues() {
        let (mut client, events) = fake_agent();
        client.open_existing("old", vec![]).unwrap();
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            ["user:earlier question", "text:earlier answer"]
        );
        assert!(client.has_session());
        let outcome = client.prompt("hello").unwrap();
        assert_eq!(outcome.text, "echo: hello");
    }
}
