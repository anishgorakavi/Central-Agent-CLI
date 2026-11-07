//! End-to-end tests: real binaries speak MCP -> A2A -> ACP to a custom agent whose "Ollama" is a local fake HTTP server.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

const CENTRAL: &str = env!("CARGO_BIN_EXE_central");
const TOKEN: &str = "test-token";

/// Starts a fake Ollama that streams "Hello " and "world" for any request; returns its base URL.
fn fake_ollama() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            let chunks = [
                r#"{"message":{"content":"Hello "},"done":false}"#,
                r#"{"message":{"content":"world"},"done":false}"#,
                r#"{"message":{"content":""},"done":true}"#,
            ];
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{}\n",
                chunks.join("\n")
            )
            .unwrap();
        }
    });
    url
}

/// Writes a config with one custom agent named `local` pointing at the fake server.
fn config_file(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("config.json");
    let config = format!(
        r#"{{"agents": {{"local": {{"kind": "ollama", "model": "m", "base_url": "{}"}}}}}}"#,
        fake_ollama()
    );
    std::fs::write(&path, config).unwrap();
    path
}

/// Starts `central a2a-serve` and returns the child plus its base URL.
fn start_a2a_server(config: &Path) -> (Child, String) {
    let mut server = Command::new(CENTRAL)
        .args([
            "--config",
            config.to_str().unwrap(),
            "a2a-serve",
            "--token",
            TOKEN,
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(server.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    let url = line
        .trim()
        .strip_prefix("listening on ")
        .expect("server announces its address")
        .to_owned();
    (server, url)
}

/// Stops a spawned process and reaps it.
fn stop(mut child: Child) {
    child.kill().unwrap();
    child.wait().unwrap();
}

// The agent card is public and points at the agent's JSON-RPC endpoint.
#[test]
fn serves_agent_card() {
    let (server, url) = start_a2a_server(&config_file("card"));
    let card: Value = ureq::get(&format!("{url}/agents/local/.well-known/agent-card.json"))
        .call()
        .unwrap()
        .into_json()
        .unwrap();
    stop(server);
    assert_eq!(card["name"], "local");
    assert_eq!(card["supportedInterfaces"][0]["protocolBinding"], "JSONRPC");
    assert_eq!(
        card["supportedInterfaces"][0]["url"],
        format!("{url}/agents/local")
    );
}

// SendMessage without the bearer token must be refused before any agent is started.
#[test]
fn rejects_missing_token() {
    let (server, url) = start_a2a_server(&config_file("auth"));
    let result = ureq::post(&format!("{url}/agents/local"))
        .send_json(json!({"jsonrpc": "2.0", "id": 1, "method": "SendMessage"}));
    stop(server);
    assert!(matches!(result, Err(ureq::Error::Status(401, _))));
}

// The MCP `delegate` tool must reach another agent through A2A -> ACP and return its reply (the Claude -> Codex path).
#[test]
fn mcp_delegate_goes_through_a2a() {
    let (server, url) = start_a2a_server(&config_file("mcp"));
    let mut bridge = Command::new(CENTRAL)
        .args(["mcp", "--a2a-url", &url, "--caller", "claude"])
        .env("CENTRAL_A2A_TOKEN", TOKEN)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = bridge.stdin.take().unwrap();
    let mut stdout = BufReader::new(bridge.stdout.take().unwrap());
    let call = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"delegate","arguments":{"agent":"local","prompt":"hi"}}}"#;
    writeln!(stdin, "{call}").unwrap();
    let mut reply = String::new();
    stdout.read_line(&mut reply).unwrap();
    stop(bridge);
    stop(server);
    assert!(
        reply.contains("Hello world") && reply.contains(r#""isError":false"#),
        "{reply}"
    );
}
