# Small core, reliable execution

Piko 0.5 keeps the native ACP runtime and its existing tools. No new runtime
libraries, framework, background service or model download is required.

## Research and choices

The review used primary implementations and protocol documentation:

- [Pi system prompts](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/system-prompt.ts):
  describe the tools and capabilities actually available. Piko keeps a stable,
  compact base prompt and adds code, read-only and delegation rules as needed.
- [Pi read tool](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/tools/read.ts):
  make truncation and continuation explicit. Piko streams bounded numbered pages,
  including beyond 1 MiB, and keeps continuation metadata inside the JSON budget.
- [Pi edit tool](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/tools/edit.ts):
  precise original-text matches matter. Piko rejects repeated or overlapping
  matches and invalid UTF-8, preserves line endings and permissions, and checks
  for changes after approval. Failed matches instruct the model to read again.
- [Pi session lifecycle](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/src/core/agent-session.ts):
  separate durable history from the context sent to the model; make retries
  abortable. Piko projects whole turns into its window without deleting saved
  turns. It retries only a model request before stream consumption begins.
- [AGENTS.md scope](https://agents.md/): prefer the closest applicable project
  guidance. Piko loads root instructions and tells the model to inspect guidance
  along a target file's workspace ancestors before editing.
- [reqwest request cloning](https://docs.rs/reqwest/0.12.28/reqwest/struct.RequestBuilder.html#method.try_clone)
  and [Tokio blocking work](https://docs.rs/tokio/1.53.3/tokio/task/fn.spawn_blocking.html):
  encode a Responses request once, share its bytes through bounded retries,
  release them after headers, and batch atomic checkpoint I/O outside the event loop.

These are independently implemented choices, not copied prompts or a Pi
compatibility layer. The built-in prompt asks the agent to gather evidence,
finish authorized work, respect project dependencies and verify its changes.
A prompt cannot guarantee model judgment or correct code.

## Failure behavior

The executor validates a complete tool-call batch before permitting side effects.
Writes and shell commands still require client approval; a denial is final.
Read-only sessions cannot mutate through tools, MCP or descendants.

At the first tool boundary, user intent and pending calls are saved atomically;
completed tool groups update that active-turn checkpoint before further execution. It contains the current turn, not all prior history.
After a completed turn, the base conversation is committed and its checkpoint
is retired. A reply with no tool actions needs only the final conversation save;
a process stopped during its initial model request may leave that prompt unsaved. On load, unfinished calls become interrupted results and are never
executed automatically. An interrupted action may already have changed the host;
inspect its state before deliberately retrying it. This is recovery bookkeeping,
not an exactly-once transaction across arbitrary shell programs or MCP servers.

Conversation storage is capped at 16 MiB per session. Model context has a separate
configurable bound. Reaching storage capacity returns an error and preserves
existing disk history; a failed final commit unloads the session for explicit
recovery. A full checkpoint remains recoverable if the process stops before commit.
Older releases already discarded some history; an update cannot restore it.

Transient connection failures and HTTP 408/429/500/502/503/504 receive at most
three retries within one request deadline, including queueing, auth and streaming.
Short numeric Retry-After values are honored. Longer or date-form advice returns
a retry-later error. Cancellation interrupts network waits and backoff. A stream
that has started is never automatically replayed. Error bodies and credential-bearing
transport URLs are not forwarded to the model or client.

## Validation

`cargo test --locked --all-targets`, `tests/stdio.py` and `tests/reliability.py`
exercise malformed tool batches, edit conflicts and encoding, bounded file pages,
checkpoint recovery, retained history, interrupted streams, retry deadlines and
cancellation. CI runs the same checks on macOS/Linux/Windows, arm64/x64.

[Measurements and reproduction](performance.md) distinguish synthetic local
overhead from model inference, provider capacity and coding quality.


## Tool expansion in 0.6

Built-in schemas are immutable and shared across sessions by mode and delegation
capability. Their encoded Responses representation is generated once and copied
into outgoing requests without walking a JSON tree per step. MCP schemas remain
session-specific and are refreshed through the existing lazy MCP contract.

Web tools use existing reqwest/rustls; fetch pins public DNS and rechecks redirects.
Search is an optional [Brave HTTP API](https://api-dashboard.search.brave.com/documentation/guides/authentication)
connection, not an implicit scraping proxy. Browser control uses the
[Chrome DevTools Protocol](https://chromedevtools.github.io/devtools-protocol/)
over loopback with an explicitly configured dedicated profile. No Chromium,
Playwright or browser cache is shipped. Evaluation handles and failed new tabs
have bounded cleanup guards.

Interactive terminals use WezTerm's [portable-pty](https://docs.rs/portable-pty/0.9.0/portable_pty/)
for Unix PTY / Windows ConPTY portability; no Node terminal server is required.
Only live terminals allocate their two small I/O stacks. Inputs, output channels,
rolling logs, job count and lifetimes are bounded. Parent-to-child mailboxes hold
at most eight 8 KiB messages. Finishing and accepting messages share one lock so
late steering cannot silently pass the final-reply boundary.

Exact patches and image viewing reuse Piko's existing path and multimodal rules.
Patch batches preflight completely, then recheck after permission; file replacement
is atomic but multi-file rollback is deliberately not promised. Standard
[ACP tool progress, permissions and image content](https://agentclientprotocol.com/protocol/v1/tool-calls)
keep these tools usable in CodeInk and other ACP clients.
