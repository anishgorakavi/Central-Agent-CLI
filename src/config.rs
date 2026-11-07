//! Agent registry: built-in ACP agents, user-defined custom agents, and the JSON config file that stores them.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";
const DEFAULT_OPENAI_URL: &str = "https://api.openai.com/v1";
const CLAUDE_DELEGATE_MODEL: &str = "claude-sonnet-5-5";
const DEFAULT_OLLAMA_MODEL: &str = "llama3.2";
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
    /// The model a custom LLM agent runs; None for ACP agents, which choose their own.
    pub fn model(&self) -> Option<&str> {
        match self {
            AgentSpec::Ollama { model, .. } | AgentSpec::Openai { model, .. } => Some(model),
            AgentSpec::Acp { .. } => None,
        }
    }

    /// Returns a copy of this spec that runs `model` instead of its configured one.
    /// Custom LLM agents swap their model; the Claude and Codex adapters take it through their
    /// environment / `-c` flag. Other ACP agents have no known model switch, so they are rejected.
    pub fn with_model(&self, agent: &str, model: &str) -> Result<AgentSpec> {
        let mut spec = self.clone();
        match (&mut spec, agent) {
            (
                AgentSpec::Ollama { model: current, .. } | AgentSpec::Openai { model: current, .. },
                _,
            ) => {
                *current = model.to_owned();
            }
            (AgentSpec::Acp { env, .. }, "claude") => {
                env.insert("ANTHROPIC_MODEL".to_owned(), model.to_owned());
            }
            (AgentSpec::Acp { args, .. }, "codex") => {
                args.extend(["-c".to_owned(), format!("model=\"{model}\"")]);
            }
            _ => bail!("agent '{agent}' does not support choosing a model"),
        }
        Ok(spec)
    }

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

/// Built-in agents: ACP adapters installed from upstream on first use, and the Ollama harness.
fn builtin(name: &str) -> Option<AgentSpec> {
    if name == "ollama" {
        // OLLAMA_MODEL lets users pick the tab's model without a config file; delegations can override it.
        return Some(AgentSpec::Ollama {
            model: std::env::var("OLLAMA_MODEL")
                .unwrap_or_else(|_| DEFAULT_OLLAMA_MODEL.to_owned()),
            base_url: default_ollama_url(),
            system: None,
        });
    }
    let (command, args): (&str, &[&str]) = match name {
        "claude" => ("npx", &["-y", "@agentclientprotocol/claude-agent-acp"]),
        "codex" => ("npx", &["-y", "@agentclientprotocol/codex-acp"]),
        "antigravity" | "agy" => ("agy", &["acp"]),
        _ => return None,
    };
    // The Claude adapter otherwise falls back to a small default model; delegated work should run on
    // Sonnet unless the user already pinned a model in their environment.
    let mut env = BTreeMap::new();
    if name == "claude" && std::env::var_os("ANTHROPIC_MODEL").is_none() {
        env.insert(
            "ANTHROPIC_MODEL".to_owned(),
            CLAUDE_DELEGATE_MODEL.to_owned(),
        );
    }
    Some(AgentSpec::Acp {
        command: command.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        env,
    })
}

const BUILTIN_NAMES: [&str; 4] = ["claude", "codex", "antigravity", "ollama"];

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
                // USERPROFILE is the Windows equivalent of HOME.
                let home = std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .context("HOME is not set; pass --config")?;
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

    // Model overrides land where each kind reads its model, and unsupported agents fail loudly.
    #[test]
    fn with_model_overrides_per_agent_kind() {
        let config = Config::default();
        let claude = config
            .resolve("claude")
            .unwrap()
            .with_model("claude", "m1")
            .unwrap();
        assert!(matches!(&claude, AgentSpec::Acp { env, .. } if env["ANTHROPIC_MODEL"] == "m1"));
        let codex = config
            .resolve("codex")
            .unwrap()
            .with_model("codex", "m2")
            .unwrap();
        assert!(
            matches!(&codex, AgentSpec::Acp { args, .. } if args.ends_with(&["-c".into(), "model=\"m2\"".into()]))
        );
        let ollama = config
            .resolve("ollama")
            .unwrap()
            .with_model("ollama", "qwen")
            .unwrap();
        assert!(matches!(&ollama, AgentSpec::Ollama { model, .. } if model == "qwen"));
        assert!(
            config
                .resolve("antigravity")
                .unwrap()
                .with_model("antigravity", "x")
                .is_err()
        );
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
