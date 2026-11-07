//! Command lines for running the real Claude Code, Codex and Antigravity CLIs in embedded terminals, wired to the A2A bridge.
use crate::config::Launch;
use crate::mcp_server::{TOKEN_ENV, bridge_args};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;

/// Everything needed to point a CLI's MCP client at this app's delegation bridge.
pub struct Bridge<'a> {
    pub exe: &'a Path,
    pub config_path: &'a Path,
    pub a2a_url: &'a str,
    pub token: &'a str,
    pub image_agent: &'a str,
}

/// True for agents that have a real CLI to embed (as opposed to chat-only custom agents).
pub fn is_native(name: &str) -> bool {
    matches!(name, "claude" | "codex" | "antigravity" | "agy")
}

/// Builds the launch command for agent `name`, or None if it has no native CLI.
/// With `resume`, the CLI starts in its own "continue a past conversation" mode.
///
/// Claude and Codex get the bridge injected per run (no config files are touched), so Claude can call
/// `generate_image`/`delegate`. Antigravity only supports persistent MCP registration (`agy mcp add`),
/// which this app does not do on the user's behalf, so it runs without the bridge.
pub fn launch(name: &str, bridge: &Bridge, resume: bool) -> Option<Launch> {
    let args = bridge_args(bridge.config_path, bridge.a2a_url, name, bridge.image_agent);
    let (program, args) = match name {
        "claude" => {
            let config = json!({"mcpServers": {"central": {
                "command": bridge.exe,
                "args": args,
                "env": {TOKEN_ENV: bridge.token},
            }}});
            let mut cli = vec!["--mcp-config".to_owned(), config.to_string()];
            if resume {
                cli.push("--resume".to_owned());
            }
            ("claude", cli)
        }
        "codex" => {
            // Codex reads `-c key=value` overrides as TOML; JSON string/array syntax is valid TOML here.
            let toml_string = |value: &str| json!(value).to_string();
            let toml_args = json!(args).to_string();
            let mut cli = vec![
                "-c".to_owned(),
                format!(
                    "mcp_servers.central.command={}",
                    toml_string(&bridge.exe.to_string_lossy())
                ),
                "-c".to_owned(),
                format!("mcp_servers.central.args={toml_args}"),
                "-c".to_owned(),
                format!(
                    "mcp_servers.central.env={{{TOKEN_ENV}={}}}",
                    toml_string(bridge.token)
                ),
            ];
            if resume {
                cli.push("resume".to_owned());
            }
            ("codex", cli)
        }
        "antigravity" | "agy" => (
            "agy",
            if resume {
                vec!["--continue".to_owned()]
            } else {
                Vec::new()
            },
        ),
        _ => return None,
    };
    Some(Launch {
        program: program.to_owned(),
        args,
        env: BTreeMap::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bridge with fixed placeholder values.
    fn bridge() -> Bridge<'static> {
        Bridge {
            exe: Path::new("/bin/central"),
            config_path: Path::new("/c.json"),
            a2a_url: "http://127.0.0.1:9",
            token: "secret",
            image_agent: "codex",
        }
    }

    // Claude receives the bridge as inline --mcp-config JSON, with the caller name and token in it.
    #[test]
    fn claude_gets_inline_mcp_config() {
        let launch = launch("claude", &bridge(), false).unwrap();
        assert_eq!(launch.program, "claude");
        assert_eq!(launch.args[0], "--mcp-config");
        let config: serde_json::Value = serde_json::from_str(&launch.args[1]).unwrap();
        let server = &config["mcpServers"]["central"];
        assert_eq!(server["command"], "/bin/central");
        assert_eq!(server["env"][TOKEN_ENV], "secret");
        assert!(server["args"].to_string().contains("\"claude\""));
    }

    // Codex receives TOML -c overrides; resume appends its subcommand last.
    #[test]
    fn codex_gets_toml_overrides_and_resume() {
        let launch = launch("codex", &bridge(), true).unwrap();
        assert!(
            launch
                .args
                .contains(&"mcp_servers.central.command=\"/bin/central\"".to_owned())
        );
        assert!(launch.args.contains(&format!(
            "mcp_servers.central.env={{{TOKEN_ENV}=\"secret\"}}"
        )));
        assert_eq!(launch.args.last().unwrap(), "resume");
    }

    // Antigravity has no per-run MCP injection, and chat-only agents have no native launch at all.
    #[test]
    fn agy_has_no_bridge_and_custom_agents_none() {
        assert!(launch("agy", &bridge(), false).unwrap().args.is_empty());
        assert_eq!(
            launch("antigravity", &bridge(), true).unwrap().args,
            ["--continue"]
        );
        assert!(launch("local", &bridge(), false).is_none());
        assert!(!is_native("local"));
    }
}
