# TREE

Terminal UI (mouse-aware) that runs Claude Code, Codex, Antigravity and custom Ollama/OpenAI-compatible agents over the Agent Client Protocol, and lets agents delegate to each other over A2A (e.g. Claude asking Codex to generate an image).

Cargo.toml             Package manifest and dependencies.
.gitignore             Ignores build output and generated images.
README.md              Project overview, logo and usage.
assets/
  logo.png             Project logo, generated with Codex (model gpt-6-sol).
src/                   Application code.
  main.rs              Entry point: starts the TUI by default; subcommands agents, sessions, check, add, a2a-serve, serve, mcp; `--resume <agent>` opens the session picker.
  config.rs            Agent registry: built-in ACP agents, custom agent specs, config file load/save.
  rpc.rs               Newline-delimited JSON-RPC framing and message builders shared by ACP and MCP code.
  acp_client.rs        ACP client: spawns an agent, handshakes, runs prompts, lists/reloads past sessions (with auto-login), reports text/tool/image events, delegates permission decisions.
  a2a.rs               A2A v1.0 (JSON-RPC over HTTP): server exposing every agent as a task endpoint, and the client used by the MCP bridge.
  mcp_server.rs        MCP bridge given to agents: `delegate` and `generate_image` tools that call other agents through A2A.
  custom_agent.rs      ACP agent server that answers prompts with an LLM; the custom harness.
  llm.rs               Streaming chat requests to Ollama and OpenAI-compatible endpoints.
  event.rs             Messages from background threads to the TUI, and the permission bridge (auto-approve or ask the user).
  worker.rs            Thread owning one agent's ACP connection for a chat tab; handles prompt, list-sessions, resume and new-session commands.
  app.rs               TUI state: tabs, transcripts, input editing, key/mouse handling, /resume and /new commands, session picker, permission queue, worker startup.
  ui.rs                TUI rendering: sidebar, transcript, A2A activity pane, message box, status bar, permission dialog, session picker dialog, click targets.
  tui.rs               Terminal setup/teardown (raw mode, mouse capture) and the event loop.
tests/
  fixtures/fake_agent.py  Scripted ACP agent (login, session list/load with history replay) used by acp_client tests; needs python3.
  e2e.rs               Runs the real binary: A2A agent card and auth, and MCP -> A2A -> ACP delegation against a fake Ollama.
