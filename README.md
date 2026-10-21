# Codex Harness

A small, quiet native Rust backend for coding agents. Your ACP-compatible app
provides the UI; the harness handles streaming, file/search tools, isolated
subagents, background jobs, MCP servers and optional computer use.

## Install

```sh
pnpm add -g codex-harness
```

Or use `npm install -g codex-harness`. Requires Node.js 22.15+ for the installer
launcher. No Rust, Python, compilation, postinstall script or runtime download.
Only the native package for your OS/CPU is installed. Supports macOS, Linux and
Windows on arm64 and x64. Linux binaries use static musl and work on glibc and
musl distributions. Windows runs natively; WSL is optional.

Prefer no Node.js at all? Download the matching archive from
[Releases](https://github.com/ErzenXz/codex-harness/releases), verify it against
`SHA256SUMS`, and put the executable on PATH. `ripgrep` is recommended for faster
search; a bounded Rust fallback is built in.

## Sign in

```sh
codex-harness login
```

Open the printed link, enter the code and approve access. Enable device-code
login in ChatGPT security settings if requested. Your plan must allow the model;
subscription limits still apply. No API key is needed for subscription sign-in.

Defaults use `gpt-6-luna`. To choose reasoning, concurrency or another provider,
copy a [starter configuration](examples/) to a trusted user directory and pass
`--config /absolute/path/config.toml` to **both login and the ACP launcher**.
[API key and provider setup](docs/reference.md#authentication-and-providers).
No configuration is read from the project being edited.

## Connect your ACP app

Set its agent command to `codex-harness`, with arguments `["--acp"]`. For the
lowest overhead, get the absolute native executable path once:

```sh
codex-harness --print-binary-path
```

Use that path directly in your app's launcher:

```json
{
  "command": "/absolute/path/printed/above/codex-harness",
  "args": ["--acp"],
  "env": {}
}
```

On Windows the path ends in `.exe`. If using a custom config, append
`"--config", "/absolute/path/config.toml"` to `args`. Your ACP app sends prompts
and approvals over stdin/stdout. No files need to be installed in your projects.

A blank terminal is normal if you run the harness yourself: it waits for ACP
messages. There is no terminal chat, web server or status banner. Closed stdin
shuts it down. On macOS/Linux the npm launcher replaces itself with Rust using
Node's `execve` where available; Windows uses a child process. Direct native
launch avoids Node startup on every platform.

## Small by design

- One current-thread async runtime, pooled HTTP and lazy shared MCP connections.
- In-place streamed tool arguments and SSE fragments; no repeated whole-buffer
  copies. JSON budget checks count bytes without allocating another JSON buffer.
- Whole-turn history eviction, bounded files/streams/tool output, backpressure,
  explicit concurrency caps and session unloading.
- No telemetry, background updater, daemon or bundled browser/model/runtime.
- Permission-gated edits/shell/MCP tools, plan mode, subagent roles, cancellation,
  persistence and observed provider usage reporting stay available.

Performance figures and reproducible tests live in
[Performance](docs/performance.md). These measure local overhead; model inference,
provider limits and workload determine end-to-end agent performance.

Native desktop control requires Linux X11 and is disabled by default. For macOS
and Windows desktop/browser control, connect a trusted MCP server. Approved shell,
MCP and desktop tools can access the host; they are **not an OS sandbox**.
Credentials are stored unencrypted in private local state. See
[Security and limits](docs/reference.md#safety-boundaries).

## Update, remove, build

```sh
pnpm add -g codex-harness@latest
codex-harness logout
pnpm remove -g codex-harness
```

Restart your ACP agent after updating. If you launch the printed native path,
refresh that path after updating. Logout removes local login credentials; retained
conversations remain in the state directory. On Unix state defaults to
`$XDG_STATE_HOME/codex-harness` or `~/.local/state/codex-harness`; on Windows it is
`%LOCALAPPDATA%\codex-harness`. `CODEX_HARNESS_STATE_DIR` overrides it.

Source install (Rust 1.88+ and your OS build tools required):

```sh
cargo install --git https://github.com/ErzenXz/codex-harness --tag v0.2.0 --locked
```

Missing platform package? Reinstall with optional dependencies enabled; do not
use `--no-optional` or `--omit=optional`. There is no fallback network download.

[Technical reference](docs/reference.md) · [Development and tests](docs/reference.md#development) · [Release process](docs/releasing.md)
