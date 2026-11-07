//! Agent registry: built-in ACP agents, user-defined custom agents, and the JSON config file that stores them.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
const DEFAULT_OPENAI_URL: &str = "https://api.openai.com/v1";
const DEFAULT_IMAGE_AGENT: &str = "codex";

/// How to reach one agent: an external ACP process, or a custom harness around an LLM endpoint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AgentSpec {
    Acp {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
    Ollama {
        model: String,
        #[serde(default = "default_ollama_url")]
        base_url: String,
        system: Option<String>,
    },
    Openai {
        model: String,
        #[serde(default = "default_openai_url")]
        base_url: String,
        /// Name of the environment variable holding the key; the key itself is never stored.
        api_key_env: Option<String>,
        system: Option<String>,
    },
}

/// Serde default for Ollama's base URL when the config omits it.
fn default_ollama_url() -> String {
    DEFAULT_OLLAMA_URL.to_owned()
}

/// Serde default for the OpenAI-compatible base URL when the config omits it.
fn default_openai_url() -> String {
    DEFAULT_OPENAI_URL.to_owned()
}

/// The process to spawn so that it speaks ACP over stdio.
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

impl AgentSpec {
    /// Resolves this spec to a spawnable command. Custom agents re-invoke this binary
    /// (`serve <name>`) so they look like any other ACP agent to the client.
    pub fn launch(&self, name: &str, exe: &Path, config_path: &Path) -> Launch {
        match self {
            AgentSpec::Acp { command, args, env } => Launch {
                program: command.clone(),
                args: args.clone(),
                env: env.clone(),
            },
            AgentSpec::Ollama { .. } | AgentSpec::Openai { .. } => Launch {
                program: exe.to_string_lossy().into_owned(),
                args: vec![
                    "--config".to_owned(),
                    config_path.to_string_lossy().into_owned(),
                    "serve".to_owned(),
                    name.to_owned(),
                ],
                env: BTreeMap::new(),
            },
        }
    }
}

/// Built-in ACP agents, installed from their upstream adapters on first use.
fn builtin(name: &str) -> Option<AgentSpec> {
    let (command, args): (&str, &[&str]) = match name {
        "claude" => ("npx", &["-y", "@agentclientprotocol/claude-agent-acp"]),
        "codex" => ("npx", &["-y", "@agentclientprotocol/codex-acp"]),
        "antigravity" | "agy" => ("agy", &["acp"]),
        _ => return None,
    };
    Some(AgentSpec::Acp {
        command: command.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        env: BTreeMap::new(),
    })
}

const BUILTIN_NAMES: [&str; 3] = ["claude", "codex", "antigravity"];

/// User configuration: custom agents (which may also shadow built-ins) and the image agent choice.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(skip)]
    pub path: PathBuf,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentSpec>,
    /// Agent used by the `generate_image` tool; defaults to codex.
    pub image_agent: Option<String>,
}

impl Config {
    /// Loads the config from `path`, or from `~/.config/central/config.json` when None.
    /// A missing file yields an empty config that will be created on the first save.
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let path = match path {
            Some(path) => path,
            None => {
                let home = std::env::var_os("HOME").context("HOME is not set; pass --config")?;
                PathBuf::from(home).join(".config/central/config.json")
            }
        };
        let mut config = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        config.path = path;
        Ok(config)
    }

    /// Writes the config back to its path, creating parent directories.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", self.path.display()))
    }

    /// Looks up an agent by name; config entries take precedence over built-ins.
    pub fn resolve(&self, name: &str) -> Result<AgentSpec> {
        if let Some(spec) = self.agents.get(name).cloned().or_else(|| builtin(name)) {
            return Ok(spec);
        }
        bail!("unknown agent '{name}'; run `central agents` to list them")
    }

    /// Name of the agent that generates images for the `generate_image` tool.
    pub fn image_agent(&self) -> &str {
        self.image_agent.as_deref().unwrap_or(DEFAULT_IMAGE_AGENT)
    }

    /// All usable agent names: built-ins first, then custom ones not already listed.
    pub fn agent_names(&self) -> Vec<String> {
        let mut names: Vec<String> = BUILTIN_NAMES.iter().map(|n| (*n).to_owned()).collect();
        for name in self.agents.keys() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Omitted fields take defaults so hand-written configs stay short.
    #[test]
    fn parses_minimal_custom_agent() {
        let config: Config =
            serde_json::from_str(r#"{"agents": {"local": {"kind": "ollama", "model": "qwen"}}}"#)
                .unwrap();
        assert_eq!(
            config.agents["local"],
            AgentSpec::Ollama {
                model: "qwen".into(),
                base_url: DEFAULT_OLLAMA_URL.into(),
                system: None
            }
        );
    }

    // Config entries override built-ins of the same name; unknown names fail clearly.
    #[test]
    fn resolve_prefers_config_and_rejects_unknown() {
        let mut config = Config::default();
        assert!(config.resolve("codex").is_ok());
        assert!(config.resolve("nope").is_err());
        let override_spec = AgentSpec::Acp {
            command: "mine".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        config.agents.insert("codex".into(), override_spec.clone());
        assert_eq!(config.resolve("codex").unwrap(), override_spec);
    }

    // Custom agents launch this binary with the config path so the child sees the same registry.
    #[test]
    fn custom_agent_launches_serve_subcommand() {
        let spec = AgentSpec::Ollama {
            model: "m".into(),
            base_url: default_ollama_url(),
            system: None,
        };
        let launch = spec.launch("local", Path::new("/bin/central"), Path::new("/c.json"));
        assert_eq!(launch.program, "/bin/central");
        assert_eq!(launch.args, ["--config", "/c.json", "serve", "local"]);
    }
}
