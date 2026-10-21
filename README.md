<p align="center">
  <img src="https://raw.githubusercontent.com/ErzenXz/codex-harness/main/docs/assets/banner.svg" alt="Codex Harness — native Rust, small footprint." width="100%">
</p>

<p align="center">
  A small native Rust backend for coding agents.<br>
  macOS · Linux · Windows &nbsp;/&nbsp; arm64 · x64
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/codex-harness">npm</a> ·
  <a href="https://github.com/ErzenXz/codex-harness/releases">Downloads</a> ·
  <a href="https://github.com/ErzenXz/codex-harness/blob/main/docs/reference.md">Documentation</a>
</p>

## Quick start

```sh
pnpm add -g codex-harness
codex-harness login
```

Requires pnpm and Node.js 22.15+. Installs only your platform's native binary, with no
compilation or install scripts. [Standalone downloads](https://github.com/ErzenXz/codex-harness/releases)
run without Node.js. Linux uses static musl; Windows runs natively.

Set your ACP app's agent command:

```json
{ "command": "codex-harness", "args": ["--acp"] }
```

Your app provides the interface; the harness communicates over stdin/stdout.
Subscription login uses your plan's model access and limits.
[Other providers and API keys →](https://github.com/ErzenXz/codex-harness/blob/main/docs/reference.md#authentication-and-providers)

Available models and reasoning levels load automatically. Choose them in your
ACP app; selections persist per session. [Discovery and picker details →](https://github.com/ErzenXz/codex-harness/blob/main/docs/reference.md#model-discovery-and-reasoning)

## Performance

Measured locally on **macOS arm64 · v0.3.1**.

| Metric | Result |
| :--- | ---: |
| Native binary | **4.58 MB** |
| ACP startup + initialize | **8.18 ms** |
| Idle memory, sampled RSS | **4.02 MiB** |

One synthetic run; startup includes local fixture setup and excludes model
inference. Memory and timings vary by workload.
[Raw results](https://github.com/ErzenXz/codex-harness/blob/main/docs/benchmarks/macos-arm64-0.3.1.json) ·
[Before/after results and reproduction](https://github.com/ErzenXz/codex-harness/blob/main/docs/performance.md)

## Built to stay light

One async event-loop thread, shared HTTP connections, lazy MCP connections and
bounded history, files and tool output. No telemetry, daemon, updater or bundled
browser/model.

Streaming, file/search tools, MCP, subagents, background jobs, persistent sessions,
cancellation and permission controls are built in.

For the lowest launch overhead, run `codex-harness --print-binary-path` and use
that executable directly in your ACP app. On macOS/Linux the standard launcher
replaces Node with Rust where `execve` is available; Windows keeps a forwarding
process until you use the native path.

Approved tools can access your host. Native desktop control is optional and
requires Linux X11; macOS/Windows computer use works through MCP. See
[security and limits](https://github.com/ErzenXz/codex-harness/blob/main/docs/reference.md#safety-boundaries).

---

[Configuration](https://github.com/ErzenXz/codex-harness/tree/main/examples) ·
[Install, update & remove](https://github.com/ErzenXz/codex-harness/blob/main/docs/reference.md#installation-and-maintenance) ·
[Development](https://github.com/ErzenXz/codex-harness/blob/main/docs/reference.md#development) ·
[MIT license](https://github.com/ErzenXz/codex-harness/blob/main/LICENSE)
