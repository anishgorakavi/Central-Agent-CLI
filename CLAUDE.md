# CLAUDE.md

@AGENTS.md

## Claude Code specifics

- Use the `personal-code-hygiene` skill whenever you write, modify, review or move code or files here. Run `sh ~/.claude/skills/personal-code-hygiene/scripts/check-hygiene.sh .` before finishing, then `cargo fmt`, `cargo clippy --all-targets` and `cargo test`.
- You may be running *inside* this app (an embedded Claude tab). Then `mcp__central__delegate` and `mcp__central__generate_image` exist, but only while the TUI that hosts the A2A server is open. If a call fails to connect, the TUI was closed; do not try to start another one to fix it.
- Another `central` TUI and other Claude instances may be running on the user's machine. Do not kill processes by name pattern. Use `TaskStop` for background tasks you started and exact PIDs you can prove are yours.
- Put scratch scripts and logs in your session scratchpad directory, not in the repo or `/tmp`.
- Ask before pushing, tagging, creating repos, or doing anything that sends content to an external service. Committing and pushing were each requested explicitly in past sessions.
- When a change touches how an embedded CLI behaves (keys, mouse, resize, launch args), verify it by driving the real binary in a pty after running `cargo build`, in addition to the unit tests.
