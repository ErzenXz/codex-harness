<p align="center">
  <img src="https://raw.githubusercontent.com/ErzenXz/piko/main/docs/assets/banner.svg" alt="Piko — native Rust, small footprint." width="100%">
</p>

<p align="center">
  A tiny, powerful coding agent.<br>
  macOS · Linux · Windows &nbsp;/&nbsp; arm64 · x64
</p>

<p align="center">
  <a href="https://www.npmjs.com/package/@erzenx/piko">npm</a> ·
  <a href="https://github.com/ErzenXz/piko/releases">Downloads</a> ·
  <a href="https://github.com/ErzenXz/piko/blob/main/docs/reference.md">Documentation</a>
</p>

## Quick start

```sh
pnpm add -g @erzenx/piko
piko login
```

Requires pnpm and Node.js 22.15+. Installs only your platform's native binary, with no
compilation or install scripts. [Standalone downloads](https://github.com/ErzenXz/piko/releases)
run without Node.js. Linux uses static musl; Windows runs natively.

Set your ACP app's agent command:

```json
{ "command": "piko", "args": ["--acp"] }
```

Your app provides the interface; Piko communicates over stdin/stdout.
Subscription login uses your plan's model access and limits.
[Other providers and API keys →](https://github.com/ErzenXz/piko/blob/main/docs/reference.md#authentication-and-providers)

Upgrading from `codex-harness`? Install Piko and change your agent command to
`piko`; your existing login and sessions are reused automatically.

Available models and reasoning levels load automatically. Choose them in your
ACP app; selections persist per session. [Discovery and picker details →](https://github.com/ErzenXz/piko/blob/main/docs/reference.md#model-discovery-and-reasoning)

## Performance

Measured locally on **macOS arm64 · v0.4.0**.

| Metric | Result |
| :--- | ---: |
| Native binary | **4.56 MB** |
| ACP startup + initialize | **16.72 ms** |
| Idle memory, sampled RSS | **3.97 MiB** |

One synthetic run; startup includes local fixture setup and excludes model
inference. Memory and timings vary by workload.
[Raw results](https://github.com/ErzenXz/piko/blob/main/docs/benchmarks/macos-arm64-0.4.0.json) ·
[Before/after results and reproduction](https://github.com/ErzenXz/piko/blob/main/docs/performance.md)

## Built to stay light

One async event-loop thread, shared HTTP connections, lazy MCP connections and
bounded history, files and tool output. No telemetry, daemon, updater or bundled
browser/model.

Streaming, file/search tools, MCP, subagents, background jobs, persistent sessions,
cancellation and permission controls are built in. Precise edits, bounded file
pages and recoverable tool checkpoints help keep work predictable.
[Design and recovery →](https://github.com/ErzenXz/piko/blob/main/docs/design.md)

For the lowest launch overhead, run `piko --print-binary-path` and use
that executable directly in your ACP app. On macOS/Linux the standard launcher
replaces Node with Rust where `execve` is available; Windows keeps a forwarding
process until you use the native path.

Approved tools can access your host. Native desktop control is optional and
requires Linux X11; macOS/Windows computer use works through MCP. See
[security and limits](https://github.com/ErzenXz/piko/blob/main/docs/reference.md#safety-boundaries).

---

[Configuration](https://github.com/ErzenXz/piko/tree/main/examples) ·
[Install, update & remove](https://github.com/ErzenXz/piko/blob/main/docs/reference.md#installation-and-maintenance) ·
[Development](https://github.com/ErzenXz/piko/blob/main/docs/reference.md#development) ·
[MIT license](https://github.com/ErzenXz/piko/blob/main/LICENSE)
