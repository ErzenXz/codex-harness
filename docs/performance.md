# Performance

The harness is a single native Rust process with one async event-loop thread,
shared HTTP connections and on-demand blocking workers. It starts no MCP servers,
browsers or models until explicitly needed. Cap concurrency to your machine and
provider allowance; unload idle sessions with `_harness/session/unload` to release
retained history and jobs.

Version 0.2 removes repeated streamed-argument copies, appends SSE line fragments
in bulk, moves completed output, counts JSON without duplicate buffers and trims
history without cloning the result. Built-in grep reuses one bounded file buffer.
The size-optimized release profile retains thin LTO and one codegen unit.
`--help` and `--version` return before creating an async runtime or user state.

The npm launcher installs one exact-version platform binary, with no postinstall
script or network access at launch. On macOS/Linux it calls Node's `execve` when
available, replacing Node with Rust and freeing the launcher memory. On Windows
Node remains as a forwarding process. Use the path from `--print-binary-path`
directly in your ACP client to eliminate launcher overhead on every platform.

## Reproduce

```sh
cargo build --release --locked
HARNESS_BIN=target/release/codex-harness python3 tests/benchmark.py --agents 10
```

On Windows set `HARNESS_BIN` to the `.exe` path in your shell first. The benchmark
uses temporary state/workspaces and loopback mock SSE; no credentials or paid
model calls. It reports native binary bytes, ACP startup, ten simultaneous turns,
process RSS (Linux/macOS) and search over 10,000 files. Startup includes fixture
setup; first-launch OS verification and filesystem cache materially affect it.

Results do not establish a competitor ranking, model intelligence, real provider
throughput or a cache hit rate. RSS varies with platform, workload, active tools,
conversation length and configured concurrency. Context bytes bound serialized
history; they do not cap total process RSS. Read the runtime limits in the
[technical reference](reference.md#performance-and-memory).

Measurements for this release are recorded after validation below.
