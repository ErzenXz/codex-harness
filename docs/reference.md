# Technical reference

Looking to install and start the harness? Use the [quick start](../README.md).
This page covers provider settings, runtime limits, MCP, computer use, protocol,
security, development and benchmark details.

A quiet, ACP-native coding-agent runtime in Rust. No terminal chat UI, banners,
spinners, or unsolicited status text. Start the process; drive it from your ACP
client. Human-readable output is limited to explicit help, version, and login.

**An initial implementation, not a claim of market-leading performance.** Model
latency, account quotas, cache residency, and code quality remain upstream and
workload dependent. Benchmarks must separate local overhead from model inference.

## Installation and maintenance

Install the published command with `pnpm add -g codex-harness`, then run
`codex-harness login`. The launcher requires Node.js 22.15+; the native executable
itself does not require Node.js. Only your OS/CPU package is installed. Keep
optional dependencies enabled; reinstall if they were omitted. There is no
fallback network download. `ripgrep` is recommended for faster search; a bounded
Rust fallback is built in.

For a standalone installation, download your platform's archive from
[Releases](https://github.com/ErzenXz/codex-harness/releases), verify its SHA-256
against `SHA256SUMS`, then put the executable on PATH.

```sh
pnpm add -g codex-harness@latest
codex-harness logout
pnpm remove -g codex-harness
```

Restart your ACP agent after updating. If you launch the native path printed by
`codex-harness --print-binary-path`, refresh that path after updating. Logout
removes local login credentials; retained conversations remain in the state
directory. On Unix state defaults to `$XDG_STATE_HOME/codex-harness` or
`~/.local/state/codex-harness`; on Windows it is `%LOCALAPPDATA%\codex-harness`.
`CODEX_HARNESS_STATE_DIR` overrides it.

To choose reasoning, concurrency or another provider, copy a
[starter configuration](../examples/) to a trusted user directory and pass
`--config /absolute/path/config.toml` to **both login and the ACP launcher**.
No configuration is read from the project being edited.

Source installation requires Rust 1.88+ and your OS build tools:

```sh
cargo install --git https://github.com/ErzenXz/codex-harness --tag v0.2.1 --locked
```

## ACP launch contract

See the [quick start](../README.md) for installation and sign-in. An ACP launcher
uses the installed binary and an explicitly chosen, trusted user config:

```json
{"command":"/absolute/path/to/codex-harness","args":["--acp","--config","/absolute/path/to/config.toml"],"env":{}}
```

The client must keep stdin open, continuously read stdout, negotiate protocol
version 1, then send `session/new` with an absolute `cwd` and `mcpServers: []`.
All stdout is newline-delimited JSON-RPC, not Content-Length framing. Fatal
startup/transport diagnostics go to stderr. Empty stdin/EOF shuts the agent down.
The launcher owns daemonization; this executable does not fork away from stdio.

## Authentication and providers

The default model is [`gpt-6-luna`](https://developers.openai.com/api/docs/models/gpt-6-luna).
Override it with `model` in the config or `OPENAI_MODEL` in the launcher environment.
There is no automatic fallback to another model. Availability through subscription
sign-in depends on your ChatGPT plan. API token prices do not describe subscription
quotas. With GPT-6 Luna function calling over Chat Completions, set
`reasoning_effort = "none"`; Responses supports the model's reasoning modes.

### ChatGPT subscription

`codex-harness login` uses the Codex device-code flow. Enable device-code login in
your ChatGPT security settings if necessary, open the displayed OpenAI URL, and
enter the code. An eligible plan and model access are required. Subscription
limits still apply; hundreds of local agents do not bypass them.

Credentials are stored in `auth.json` under the state directory (Unix directory
mode 0700, files 0600), refreshed when needed, and never intentionally logged or
passed to shell jobs. **Tokens are stored unencrypted on disk.** Protect the
account and state directory. `logout` deletes the local file; restart running
harness processes too, since they cache credentials in memory. It does not revoke
already-issued tokens server-side. Prefer one process per credential directory:
refresh is serialized in-process, not across independent processes.

ChatGPT tokens can only be sent to the Codex backend, never a custom provider.
The backend/device flow follows the open-source Codex contract, not a general
subscription entitlement to the OpenAI Platform API. Upstream changes may require
adapter updates. Device sign-in, streamed `gpt-6-luna` low responses, a read-only
file-tool loop, and ten concurrent short ACP turns were verified on 2026-10-19.
Forced token refresh and other providers have not been live-validated.

The ChatGPT example selects low reasoning and caps simultaneous model requests
and subagents at 10. Increase those limits only within your account allowance;
concurrency limits do not impose a total-token or subscription-usage budget.

### OpenAI API key

Skip device login. Create a config outside your project (for example,
`~/.config/codex-harness/openai.toml`):

```toml
auth = "api_key"
model = "gpt-6-luna"
reasoning_effort = "low"
max_model_requests = 10
max_subagents = 10
```

Point your ACP launcher's `--config` at that file's absolute path and supply
`OPENAI_API_KEY` using the launcher's environment/secret settings. API usage is
billed separately from a ChatGPT subscription. No interactive `login` is needed.

Set `OPENAI_API_KEY` in the **launcher environment**, not a repository file.
In `auth = "auto"`, a key takes precedence over ChatGPT credentials. Use explicit
`auth = "chatgpt"` to force subscription access. ACP `authenticate` can select an
available method for the running process; it does not start an interactive login.

### Other OpenAI-compatible providers

Pass `--config /absolute/path/config.toml`; examples are in [`examples/`](../examples/).
Responses and Chat Completions SSE/function calling are supported. Native
Anthropic/Gemini wire protocols are **not** implemented. Compatible providers can
differ in tool-schema, usage-reporting, model, and token-limit support.

## Agent runtime

- Gather context → act → verify, with repository `AGENTS.md`, `CLAUDE.md`, and
  `.harness/instructions.md` instructions.
- Streamed text, plans, tool progress, and explicit approval requests over ACP.
- Isolated explore/reviewer/worker agents sharing a workspace, not parent history.
  Explore/reviewer agents and all descendants of plan-mode agents are read-only.
- Foreground/background subagents, shell jobs, task output/status, and cancellation.
- Concurrent independent reads/searches/delegations; mutations remain ordered.
- Exact-match edits with a post-approval conflict check and atomic replacement.
- Atomic persisted conversations, load/replay, and whole-turn context eviction.
  Active-turn context exhaustion stops safely; this is not semantic compaction.

### Performance and memory

A single current-thread Tokio event loop multiplexes hundreds of model streams;
there is no OS thread or process per model agent. One pooled HTTP client and
in-memory credential cache are shared. Native `rg` runs without a shell or user
config; bounded Rust `ignore`/`regex` search is the fallback if `rg` is absent.
Search respects ignore files, skips hidden files/symlinks, and applies user globs
without overriding ignored-file protections. Native search scans files up to
128 KiB; fallback up to 1 MiB, with a 20,000-entry walk cap. Narrow searches for
large repositories. Native search has a 15-second deadline.

Defaults: 256 loaded sessions, 256 concurrent subagents, 256 concurrent model
requests, 8 searches, 16 jobs per session, depth 2, 384 KiB context per agent,
32 KiB retained tool output. Model-request/search limits are **global per process**;
job limits are per session. Limits are configurable, not promises of account
capacity. Excess model/search work queues; nested subagent admission fails fast
rather than deadlocking. Completed job records are evicted when slots are needed.

Memory scales with active conversations, in-flight JSON, HTTP buffers, and jobs.
The context budget is a serialized-history limit, not a total RSS guarantee. There
is a 1 MiB inbound ACP frame limit and bounded SSE/tool/session buffers. Slow clients
apply output backpressure rather than accumulating unlimited updates.

### Cache reuse, measured honestly

Stable instruction/tool ordering, append-only exchanges, stable per-session OpenAI
`prompt_cache_key`, and ordered tool results preserve reusable prompt prefixes.
No changing timestamps, fake padding, or redundant model-side summaries are added.
Optional `prompt_cache_retention = "24h"` is sent only to the OpenAI API (not
ChatGPT/custom endpoints); enable it only for models that support it. Newer model
families can use different caching controls; unsupported settings may be rejected.

`_harness/usage` reports provider-reported input, output, cached input tokens, and
**cached-input / total-input token ratio**. `_harness/stats` aggregates only
responses with cache details; absent details are unknown (`null`), not invented
hits or zeroes. Chat Completions requests ask for streamed usage. This is a token
ratio, not the percentage of requests hitting a cache.

OpenAI caching requires repeated eligible prefixes; short prompts, changed tools,
context eviction, cache expiry, and heavy routing traffic reduce reuse. A 99%
hit rate cannot be guaranteed by a harness. See [OpenAI's caching guide](https://platform.openai.com/docs/guides/prompt-caching).

## MCP servers

Pass MCP servers in `session/new` or `session/load` (see
[`examples/mcp-session.json`](../examples/mcp-session.json)). Supported transports:
newline-delimited **stdio** and **Streamable HTTP** with JSON or SSE replies.
Legacy HTTP+SSE transport, MCP OAuth, resource/prompt discovery, sampling,
elicitation, and resumable/background SSE subscriptions are not implemented.
Use launcher-supplied environment entries or HTTP headers for authentication.
Never commit real credentials in an example or project file.

```json
{
  "cwd": "/absolute/path/project",
  "mcpServers": [
    {"name":"local","command":"/absolute/path/mcp-server","args":["--stdio"],"env":[]},
    {"name":"remote","type":"http","url":"https://mcp.example.com/mcp","headers":[]}
  ]
}
```

Connections start only on the first code-mode turn, not session creation, idle
startup, or plan-mode prompts. Identical workspace/server definitions share one
connection across sessions and worker subagents; different workspaces do not.
No MCP subprocess is created per ordinary model agent. Stdio processes and their
Unix process groups are stopped when their last session/tool snapshot is dropped.
HTTP session expiry remains server-managed. Closed stdio connections and expired
HTTP sessions (404) reconnect on a subsequent turn; interrupted tool calls are
not automatically retried, because they may already have side effects.

Initialization negotiates the protocol version, discovers paginated tools and
caches schemas for up to 60 seconds. Stdio `tools/list_changed` notifications
invalidate that cache for the next turn; tool sets stay fixed within each turn.
Model names use `mcp_<server>_<tool>_<stable-hash>` to prevent namespace collisions.
Descriptions and results are untrusted data. **Every remote tool call requires
ACP permission**, including tools marked read-only by their server. Plan,
explore and reviewer agents receive no MCP tools.

Bounds: 8 servers per session; `max_mcp_connections = 32` distinct pooled
workspace/server definitions by default (configurable 1–256); 32 tools per turn;
4 KiB per schema; 64 KiB request frames; 1 MiB replies; 64 pending stdio requests
per connection; 45-second I/O phase timeouts. Excess work fails explicitly instead
of spawning unbounded processes. Cancellation drops the local wait and sends a
best-effort MCP cancellation notification; remote side effects cannot be undone.

Stdio environments are cleared, then a small runtime allowlist and explicitly
configured entries are forwarded. HTTP requires TLS except on loopback, disables
redirects and keeps supplied credentials out of diagnostics. Configuration/env/
headers are not persisted with conversation history: resupply `mcpServers` on
load. **MCP servers are trusted executable/network integrations, not sandboxes**:
server startup itself can run arbitrary code, and inherited `HOME` still locates
user files. Only the trusted ACP client may configure them.

## Opt-in computer use

Use [`examples/computer.toml`](../examples/computer.toml), or add `computer_use = true`
to an explicitly selected launcher configuration. The bundled
[`computer-use` skill](../skills/computer-use/SKILL.md) is automatically added to
code/worker instructions when enabled. Normal runs load no computer processes,
capture no screenshots and add no computer schema/instructions to model requests.

The native `computer_use` tool requires **Linux X11**, `xdotool`, ImageMagick's
`import`, and access through `DISPLAY`/`XAUTHORITY`. Executable paths can be set via
`computer_xdotool` and `computer_import` in the trusted config. On other platforms,
use a trusted MCP computer/browser server; native Wayland/macOS/Windows drivers
are not provided. A vision-capable model/provider is required to interpret images.

| Action | Arguments |
| --- | --- |
| `screenshot` | No other arguments |
| `move`, `click`, `double_click` | `x`, `y`; optional `button`: left/middle/right |
| `type` | `text` (maximum 4096 bytes, passed literally without a shell) |
| `key` | `key`, e.g. `Return`, `Escape`, `ctrl+l` |
| `scroll` | `direction`: up/down/left/right; optional `amount`: 1–20 ticks |

All actions, **including screenshots**, require explicit ACP permission. Plan,
explore and reviewer agents cannot use them. Native desktop actions are serialized
process-wide and have bounded output and ten-second subprocess timeouts. Use one
desktop-driving session at a time, or separate displays/VMs: serialized actions
do not isolate focus across whole multi-step flows. Input dispatch is not proof
of UI success; inspect a fresh screenshot after each action.

Screenshots are JPEG, at most 1280×960 and 96 KiB; pointer coordinates refer to
that resized image and are scaled back to the native display. Over-budget images
fail explicitly. MCP tool results support bounded text, structured content,
embedded text resources and one validated PNG/JPEG image (96 KiB, at most 4096 per
dimension and 8 million pixels). Resource links are described, never fetched
automatically. Audio, blobs and unsupported/oversized images are omitted with an
explicit marker. Responses receives image tool-output blocks; Chat Completions
receives paired text tool results followed by image user content. Both ACP live
updates and session replay include retained images. Older tool images are replaced
with an explicit omission marker when they exceed half the context-byte budget.
Schemas, instructions and image data all count against the context budget.

**Desktop access is not workspace scoped or sandboxed.** Close private windows
first. Approved screenshots go to the model provider, trusted ACP client and
private, unencrypted session storage. The skill prohibits inspecting credentials,
typing secrets, bypassing denials, or submitting external/destructive actions
without explicit user intent and approval. Do not assume automatic secret
redaction. Let the human perform login and other sensitive steps themselves.

## Safety boundaries

Code mode requires explicit client approval for **every** write/edit/shell call.
Plan mode disables those tools, including in children. Permission denial is
returned to the model; persistent deny policies should also be enforced by the
client. File tools reject traversal, workspace/symlink escapes, credential paths,
and harness state. Edits are not committed automatically.
On Unix, hard-linked regular files are also excluded from file tools/search.
Approval requests include path/operation/size metadata rather than unrelated old
file contents. Model-supplied tool arguments remain visible in ACP tool updates;
the ACP client is trusted, and this is not a general-purpose secret redactor.

**This is not an OS sandbox.** An approved shell command can access the host and
network, read the user's files, and run arbitrary programs. Shell environments are
cleared and a small runtime allowlist forwarded, but `HOME` still locates user
files. Run untrusted work in a container/VM with restricted credentials. Filesystem
checks are defense in depth, not a race-proof capability sandbox. Unix shell task
process groups are killed on cancellation/timeouts; Windows uses Job Objects to
terminate contained descendants. Job assignment follows process creation, so a
very fast child can spawn a descendant before assignment. Neither mechanism is
a security sandbox. Background jobs do not survive process restart.
Prompt cancellation stops that turn and its children, not independent jobs from
earlier completed turns. Cancel retained jobs by task ID, unload their session,
or shut down the process. SIGINT/SIGTERM trigger orderly Unix shutdown.

## Protocol surface

ACP: `initialize`, `authenticate`, `session/new`, `session/load`,
`session/prompt`, `session/cancel`, `session/set_mode`, `session/set_model`; notifications via
`session/update`; approvals via `session/request_permission`.

Each session advertises its configured model. Legacy ACP model selection accepts
that model idempotently, including before every prompt; other model IDs are
rejected. Choose a different configured model through a trusted config file or
`OPENAI_MODEL`, then create a new session.

Extensions (all stdio JSON-RPC, no extra listener):

| Method | Parameters / purpose |
| --- | --- |
| `_harness/tasks` | `sessionId`: list retained jobs |
| `_harness/task/cancel` | `sessionId`, `taskId`: cancel a job and descendants |
| `_harness/session/unload` | `sessionId`: free an idle loaded session, cancel its jobs |
| `_harness/stats` | `{}`: observed provider cache-token counters |
| `_harness/task` (notification) | task state/output deltas |
| `_harness/usage` (notification) | provider usage for a completed model response |

Text and local `file:` resource-link prompts are supported. Image **tool outputs**
are supported as above; incoming image/audio prompts and client filesystem/terminal
delegation are not implemented. MCP HTTP support is advertised during initialize
(stdio is implicit in ACP); legacy SSE transport is explicitly not advertised.
History is a bounded rolling window; evicted turns cannot be reconstructed on load.

## Development

```sh
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked && python3 tests/stdio.py
```

Tests use temporary workspaces, mock desktop executables and loopback model/MCP
servers; no desktop software, paid model calls or
credentials are needed. CI does not validate live subscription eligibility,
model intelligence, real provider caching, or comparisons against other agents.

Reproduce a release-mode local-overhead/concurrency/search benchmark:

```sh
cargo build --release --locked
python3 tests/benchmark.py --agents 10
```

It reports startup, throughput/latency against a simulated 50 ms loopback model,
Linux process RSS/high-water mark or macOS sampled RSS, and end-to-end native search over 10,000 files.
Small synthetic conversations do not predict memory use for full coding sessions.

### Bounded live validation (2026-10-19)

Ten concurrent ACP sessions used ChatGPT subscription authentication with
`gpt-6-luna`, low reasoning, and one model step each. Each answered a short,
distinct source-code question from a provided excerpt; all ten completed with
the expected answer and isolated case identifier. No writes or shell calls were
approved. This was not a ten-agent repository-editing or long-running soak test.

| Measurement | Observed |
| --- | ---: |
| Total wall time | 3.133 s |
| Per-agent latency p50 / p95 (nearest rank) | 1.270 / 3.133 s |
| Idle / ten loaded sessions / peak sampled RSS | 5.07 / 5.30 / 8.46 MiB |
| Maximum observed process threads | 9 |
| Provider input / output tokens | 11,437 / 160 |
| Provider-reported cached input tokens | 0 |

A separate small, same-session read-tool/repeated-context check also reported
zero cached tokens. These observations do not establish a general cache rate.
Earlier 256-agent benchmarks were entirely loopback/synthetic and consumed no
subscription quota. Ten successful live turns do not establish 256-agent provider
capacity, sustained throughput, rate limits, or full-session memory use.
