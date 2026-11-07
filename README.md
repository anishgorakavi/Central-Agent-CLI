<p align="center">
  <img src="assets/logo.png" alt="Central Agent CLI logo" width="160">
</p>

# Central Agent CLI

A terminal UI that embeds the real Claude Code, Codex and Antigravity CLIs (mascot and all) in tabs, and lets agents delegate work to each other over A2A. For example, Claude can ask Codex to generate an image. Custom Ollama/OpenAI-compatible agents run as chat tabs over the Agent Client Protocol (ACP).

## Usage

```sh
cargo run                       # open the TUI
cargo run -- --resume claude    # start Claude in its own resume picker (claude --resume)
cargo run -- agents             # list agents
cargo run -- add local --kind ollama --model qwen3.5:2b-mlx
```

Inside the TUI, click an agent in the sidebar to switch. In an embedded CLI every key goes to the program (Ctrl+C interrupts Claude). Ctrl+] starts a chord: `n`/`p` switch agent, `1`-`9` pick one, `a` toggle auto-approve, `r` restart in resume mode, `q` quit. In chat tabs: Enter sends, Option+Left/Right move by word, `/resume` reopens a past session, `/new` starts fresh, Ctrl+C quits.

## Tests

```sh
cargo test
```
