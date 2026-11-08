# AGENTS.md

Guidance for AI coding agents (and humans) working on **Central Agent CLI**, a Rust terminal UI. Read `TREE.md` for a file-by-file map; this file explains how the pieces fit and what bites.

## What this is

`central` is a mouse-aware TUI (ratatui) with one tab per agent:

- **Native tabs** (`claude`, `codex`, `antigravity`): the real CLI runs in a pseudo-terminal and is drawn through a vt100 emulator, so the genuine Claude Code screen (mascot included) shows up.
- **Chat tabs** (`ollama`, custom Ollama/OpenAI-compatible agents, any user-defined ACP agent): an in-app transcript driven over ACP.
- **Delegation**: one agent can ask another to do work. The headline use case is Claude asking Codex to generate an image, because Claude cannot.

## Commands

```sh
cargo build                 # debug binary at target/debug/central
cargo test                  # unit tests + tests/e2e.rs (needs python3 for tests/fixtures/fake_agent.py)
cargo clippy --all-targets  # must stay warning-free
cargo fmt                   # rustfmt is the formatter; run it before finishing
cargo run                   # open the TUI
```

`cargo test` does **not** rebuild `target/debug/central`. If you drive the real binary from a script (for example a pty harness), run `cargo build` first or you will test a stale binary.

## Architecture

Three protocols, each with one owner file:

| Protocol | Role here | Where |
|----------|-----------|-------|
| ACP (Agent Client Protocol, JSON-RPC over stdio) | `central` is the client of chat-tab agents and of delegated agents; `central serve <name>` is an ACP *server* for custom LLM agents | `acp_client.rs`, `custom_agent.rs` |
| A2A v1.0 (JSON-RPC over HTTP, blocking `SendMessage` only) | How agents talk to each other: a server in the TUI process exposes every agent at `/agents/<name>` | `a2a.rs` |
| MCP (stdio) | How an embedded Claude/Codex reaches A2A: `central mcp` exposes the `delegate` and `generate_image` tools | `mcp_server.rs` |

The delegation path, end to end:

```
embedded claude --MCP--> central mcp (bridge) --A2A HTTP + bearer token--> A2A server in the TUI process
    --ACP--> headless codex (adapter) --> reply text + raw image parts --> bridge saves files --> claude
```

- The TUI process starts the A2A server on a random loopback port with a random bearer token (`a2a::start`, `a2a::new_token`). The token reaches the bridge through the `CENTRAL_A2A_TOKEN` env var, never argv.
- Claude gets the bridge via inline `--mcp-config`; Codex via `-c mcp_servers.central.*` overrides (`terminal_agents.rs`). Antigravity gets no bridge (it only supports persistent `agy mcp add`).
- Delegated agents are spawned with **no** MCP servers so they cannot delegate onward in a loop.
- Images come back as A2A `raw` (base64) parts. The bridge saves them using only the final path component of the remote-supplied filename.
- Permission requests from any ACP agent go through `event::decider`: auto-approve while the status-bar flag is on, otherwise a TUI dialog the user answers; headless with no flag means cancel.

Threads: the main thread runs the event loop (`tui.rs`). Each chat tab has a worker thread (`worker.rs`) owning one `acp_client::Client`. Each A2A request runs on its own thread. Everything reaches the UI as `event::UiEvent` over an mpsc channel; blocking calls (permission answers) use a per-request reply channel.

## Conventions (the owner's personal code hygiene rules)

These come from the `personal-code-hygiene` skill and are mandatory:

- **`TREE.md` is kept in sync** in the same change that adds, removes, moves, renames or repurposes a file. One or two accurate lines per source file; no generated files or build output.
- Every source file opens with a one- or two-line `//!` purpose header that matches its `TREE.md` entry.
- Every function and method has a `///` doc comment (what it is for; parameters, errors or side effects only when a caller needs them). Test helpers too; tests carry a short `//` comment stating what behavior they pin down.
- Inline comments explain *why*, never what. No conversational text, commented-out code, debug leftovers or stray TODOs.
- Spaces, not tabs. rustfmt decides formatting; no unrelated churn.
- Smallest change that works. No new dependency, wrapper, trait or abstraction without a real reason. Do not change public behavior or the config schema unless asked.
- Add or update tests when behavior changes, and run them. Report honestly what you could not verify.

## Gotchas learned the hard way

- **Never write to stdout or stderr while the TUI is up.** It corrupts the screen. `rpc::read_message` silently skips non-JSON lines for this reason, and `acp_client` sends child stderr to `/dev/null`. Surface problems through `UiEvent`s or transcript entries.
- **Ctrl+] arrives from crossterm as `Char('5')` + CONTROL** (bytes 0x1c-0x1f map to Ctrl+4..7). Match both forms; `keys::encode_key` maps them back to the raw byte.
- **In a native tab every key belongs to the embedded program**, including Ctrl+C. The app's own commands use the Ctrl+] chord (`app::native_key`). Do not add bare-key shortcuts there.
- **vt100 0.18 needs non-zero sizes.** `pty_pane::non_zero` clamps; a collapsed layout can briefly produce 0.
- **Quitting must not hang.** `PtyPane::drop` SIGKILLs the child because portable-pty's own kill sends SIGHUP, which Claude Code survives.
- **The pty size must not depend on activity.** The A2A activity strip under a native tab is a fixed one-line row so Claude is not reflowed when delegations happen.
- **Chat-tab ACP prompts block their worker**, so `session/cancel` is not honored mid-reply. A2A is blocking `SendMessage` only (no streaming, no `GetTask`/`CancelTask`).
- The built-in names are in `config::BUILTIN_NAMES`; a config entry with the same name overrides a built-in (and turns a native tab into a chat tab).
- Agent adapters (`@agentclientprotocol/claude-agent-acp`, `codex-acp`) are fetched with `npx -y` on first use. They are not available offline and are not exercised by the test suite.
- Windows support is by `cfg` gates (for example the libc SIGKILL is Unix-only); keep new code portable.

## Testing notes

- Unit tests live next to the code. `tests/e2e.rs` runs the real binary against a fake Ollama (a `TcpListener` in the test) and covers the agent card, bearer-token auth, and MCP -> A2A -> ACP delegation.
- `tests/fixtures/fake_agent.py` is a scripted ACP agent (login, session list/load with history replay) used by `acp_client` tests.
- UI tests render into ratatui's `TestBackend`; pty tests run `sh -c ...` in a real pseudo-terminal.
- To verify the TUI itself, script it in a pty (Python's `pty` module works), send keystrokes and mouse escape sequences, and inspect the output. Keep such scripts in a scratch directory, not in the repo.
- The real Claude, Codex and Antigravity CLIs are not run by the suite; they need the user's logins. Say so when you have not exercised them.

## Safety

- The user may have their own `central`, `claude` and `codex` processes running. Never `pkill -f` by pattern; stop only PIDs or background tasks you started.
- Auto-approve is off by default. Do not turn it on by default or weaken the bearer-token check on the A2A server.
- Do not call image generation, delegate to real agents, or send prompts to real model accounts just to test something unless the user asked; handshakes and `/mcp`-style checks are enough.

## Git

- Commit only when asked. Do not commit `SESSION_ID.txt` (excluded locally via `.git/info/exclude`), `.DS_Store`, screenshots, or `generated-images/`.
- End commit messages with the `Co-Authored-By` line given by your harness.
- A `v*` tag triggers `.github/workflows/release.yml`, which builds and publishes binaries; do not push tags unless asked.
