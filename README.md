# Codex Harness

A quiet, ACP-native coding-agent runtime in Rust. No terminal chat UI, banners,
spinners, or unsolicited status text. Start the process; drive it from your ACP
client. Human-readable output is limited to explicit help, version, and login.

**An initial implementation, not a claim of market-leading performance.** Model
latency, account quotas, cache residency, and code quality remain upstream and
workload dependent. Benchmarks must separate local overhead from model inference.

## Run

Requires Rust 1.88+ and, preferably, [ripgrep](https://github.com/BurntSushi/ripgrep).

```sh
cargo build --release --locked
./target/release/codex-harness login # explicit interactive ChatGPT device sign-in
./target/release/codex-harness --acp # stdio; intentionally silent until ACP input
```

An ACP launcher can use:

```json
{"command":"/absolute/path/to/codex-harness","args":["--acp","--config","/absolute/path/to/config.toml"],"env":{}}
```

The client must keep stdin open, continuously read stdout, negotiate protocol
version 1, then send `session/new` with an absolute `cwd` and `mcpServers: []`.
All stdout is newline-delimited JSON-RPC, not Content-Length framing. Fatal
startup/transport diagnostics go to stderr. Empty stdin/EOF shuts the agent down.
The launcher owns daemonization; this executable does not fork away from stdio.

## Authentication and providers

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
adapter updates. Live subscription login/model calls have not yet been verified
for this repository.

### OpenAI API key

Set `OPENAI_API_KEY` in the **launcher environment**, not a repository file.
In `auth = "auto"`, a key takes precedence over ChatGPT credentials. Use explicit
`auth = "chatgpt"` to force subscription access. ACP `authenticate` can select an
available method for the running process; it does not start an interactive login.

### Other OpenAI-compatible providers

Pass `--config /absolute/path/config.toml`; examples are in [`examples/`](examples/).
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

## Safety boundaries

Code mode requires explicit client approval for **every** write/edit/shell call.
Plan mode disables those tools, including in children. Permission denial is
returned to the model; persistent deny policies should also be enforced by the
client. File tools reject traversal, workspace/symlink escapes, credential paths,
and harness state. Edits are not committed automatically.

**This is not an OS sandbox.** An approved shell command can access the host and
network, read the user's files, and run arbitrary programs. Shell environments are
cleared and a small runtime allowlist forwarded, but `HOME` still locates user
files. Run untrusted work in a container/VM with restricted credentials. Filesystem
checks are defense in depth, not a race-proof capability sandbox. Unix shell task
process groups are killed on cancellation/timeouts; Windows process-tree isolation
is not implemented. Background jobs do not survive process restart.

## Protocol surface

ACP: `initialize`, `authenticate`, `session/new`, `session/load`,
`session/prompt`, `session/cancel`, `session/set_mode`; notifications via
`session/update`; approvals via `session/request_permission`.

Extensions (all stdio JSON-RPC, no extra listener):

| Method | Parameters / purpose |
| --- | --- |
| `_harness/tasks` | `sessionId`: list retained jobs |
| `_harness/task/cancel` | `sessionId`, `taskId`: cancel a job and descendants |
| `_harness/session/unload` | `sessionId`: free an idle loaded session, cancel its jobs |
| `_harness/stats` | `{}`: observed provider cache-token counters |
| `_harness/task` (notification) | task state/output deltas |
| `_harness/usage` (notification) | provider usage for a completed model response |

Text and local `file:` resource-link prompts are supported. Images/audio, client
filesystem/terminal delegation, and MCP server connections are not implemented;
non-empty `mcpServers` is rejected rather than silently ignored. History is a
bounded rolling window; evicted turns cannot be reconstructed on load.

## Development

```sh
cargo fmt --all -- --check
cargo check --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
cargo build --locked && python3 tests/stdio.py
```

Tests use temporary workspaces and loopback SSE mocks; no paid model calls or
credentials are needed. CI does not validate live subscription eligibility,
model intelligence, real provider caching, or comparisons against other agents.
