> [!IMPORTANT]
> Remove this line to confirm you've reviewed this PR before submitting.

# Nidavellir

A hard fork of [Zed](https://zed.dev), the high-performance, multiplayer code editor built by the creators of [Atom](https://github.com/atom/atom) and [Tree-sitter](https://github.com/tree-sitter/tree-sitter).

> **The name.** Nidavellir is the realm of the master smiths in Norse mythology. This fork is a forge of its own: it takes Zed upstream and layers a revived agent **debugger tool** and a set of agent-orchestration features on top.

> **Hard-fork notice.** This repository is a hard fork of [`zed-industries/zed`](https://github.com/zed-industries/zed). It no longer tracks upstream `main` as a merge/rebase target; worthwhile upstream changes are cherry-picked in. The fork's core, fork-specific work is the revived agent **debugger tool** (`revive-debugger-tool`). AI-assisted contributions are governed by [AI_POLICY.md](./AI_POLICY.md).

> **AI disclosure.** The majority of this fork was developed using DeepSeek V4 Pro, with assistance from Claude Sonnet 4.6 and Gemini 3.1 Pro.

## What is Zed?

Zed is a high-performance, multiplayer code editor written in Rust: a native AI agent, Vim and VS Code key bindings, a built-in terminal, real-time collaboration, and a fast GPUI-based UI. For the full picture — features, downloads, and documentation — see [zed.dev](https://zed.dev) and the [upstream repository](https://github.com/zed-industries/zed). This README focuses on what **Nidavellir adds on top of upstream Zed**.

## What Nidavellir adds

The fork keeps Zed's full editor surface and adds a working, agent-driven debugger plus a set of agent-orchestration features that upstream Zed does not ship. Everything else — the editor, terminal, filesystem tools, language-server code intelligence, web search, fetch, sub-agents, and the SKILL.md system — is inherited from upstream.

### Agent debugger tool (flagship)

A revived DAP debugger that the agent drives directly: set breakpoints, step, inspect variables, read memory, and control execution through the agent instead of by hand.

- **Adapters:** Python, Go, JavaScript, TypeScript, Rust, and C/C++
- **Control parity with the debugger UI:** step in/out/back, run-to-line, pause, detach, restart, and restart-frame
- **Breakpoints:** source, data, and exception breakpoints, plus logpoints
- **Inspection:** variable snapshots, watch expressions, raw memory reads, and per-frame evaluation
- **History:** historic session snapshots and stack-frame selection

### Agent orchestration

- **Persistent memory** — `remember` / `recall` / `forget` across threads and sessions
- **Thread search** — search and read past agent threads to re-establish context across sessions and projects
- **Cross-agent coordination** — `wait_for_report` blocks on a launched build's verification report so one agent can hand off to another
- **Ask-user** — the agent can pause and elicit a decision from you
- **Window control** — the agent can drive the UI directly (dev-channel only)

### Browser, network, and containers

- **Browser automation** — the agent drives a real Chromium session over CDP: navigate, click, type, snapshot, screenshot, and read console and network activity
- **Network capture** — HAR export/import, throttling, offline emulation, request interception, and cookie/header control
- **Container ops** — run Docker Compose or systemd operations on a configured remote host over SSH

### Modes and workflows

- **Plan mode** — a per-thread overlay that routes the model through the `plan-manager` skill and the `plans/` workflow
- **REA mode** — a per-thread reverse-engineering overlay that exposes the `rea` MCP context server and the `reverse-engineer-anything` skill
- **Headless runner** — drive the agent from the command line (`eval_cli --prompt` / `--print-response`)
- **Automations** — scheduled, recurring headless agents

### Safety and permissions

- Per-tool permission policy (`allow_once`, `allow_thread`, `allow_always`, `deny`) with a restricted-workspace mode
- MCP context-server registry with per-tool enablement
- OS-level sandboxing for terminal and tool execution

## Branches and builds

- `main` — mirror of upstream Zed
- `dev` — the fork's integration branch, where fork-specific work lives
- `feature` and `wip/*` — development branches

Building works the same as upstream:

- [Building Zed for macOS](./docs/src/development/macos.md)
- [Building Zed for Linux](./docs/src/development/linux.md)
- [Building Zed for Windows](./docs/src/development/windows.md)

## AI policy and evidence

AI-assisted contributions are welcome and governed by [AI_POLICY.md](./AI_POLICY.md). In short: disclose the primary author and model(s), and attach a full acceptance run from the debugger harness — reproducible at [zed-debugger-demo-harness](https://github.com/whitecat1331/zed-debugger-demo-harness).

## Upstream relationship

This fork is deliberately divergent: it does not track upstream `main` as a merge or rebase target. Worthwhile upstream changes are cherry-picked in selectively, and portable fixes from the fork are trickled back upstream as small, standalone PRs. See [CONTRIBUTING.md](./CONTRIBUTING.md) for contribution guidance.

## License

Zed source code is licensed primarily under GPL-3.0-or-later, with Apache-2.0 components where marked.

License information for third-party dependencies must be correctly provided for CI to pass. We use [`cargo-about`](https://github.com/EmbarkStudios/cargo-about) to automatically comply with open source licenses. If CI is failing, check the following:

- Is it showing a `no license specified` error for a crate you've created? If so, add `publish = false` under `[package]` in your crate's Cargo.toml.
- Is the error `failed to satisfy license requirements` for a dependency? If so, first determine what license the project has and whether this system is sufficient to comply with its requirements. If you're unsure, ask a lawyer. Once you've verified that this system is acceptable, add the license's SPDX identifier to the `accepted` array in `script/licenses/zed-licenses.toml`.
- Is `cargo-about` unable to find the license for a dependency? If so, add a clarification field at the end of `script/licenses/zed-licenses.toml`, as specified in the [cargo-about book](https://embarkstudios.github.io/cargo-about/cli/generate/config.html#crate-configuration).
