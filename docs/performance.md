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

## macOS arm64 measurements (2026-10-21)

Same Mac, Rust 1.99, baseline `6f55af7` versus 0.2.0. Byte counts refer to
stripped native release binaries. Startup uses 31 fresh `--version` processes;
streamed-argument timings are the median of nine local runs.

[Raw measurements and individual samples](benchmarks/macos-arm64-0.2.0.json)
include both binaries' hashes, the test environment, available samples and
summary statistics.

| Measurement | Before | After |
| --- | ---: | ---: |
| Native binary bytes | 4,803,360 | 4,475,120 |
| Version startup p50 | 6.01 ms | 5.82 ms |
| Version startup p95 | 10.74 ms | 9.65 ms |
| Append 8,000 × 32-byte argument deltas | 58.24 ms | 2.02 ms |
| Idle sampled RSS | 3.80 MiB | 4.02 MiB |
| RSS after ten synthetic turns | 6.84 MiB | 6.58 MiB |
| Ten concurrent mock turns, wall time | 73 ms | 67 ms |

The binary is 6.8% smaller. The isolated streamed-argument hot path is about 29×
faster for this 256,000-byte fixture; this is **not a 29× end-to-end agent speedup**.
The ten-session figures are one synthetic run per binary with 50 ms mock model
latency, small prompts and filesystem persistence. The idle RSS difference shows
that lower allocation churn does not guarantee lower RSS in every phase.

Reproduce the stream hot path with:

```sh
cargo run --release --locked --example stream_benchmark
```

`--version` startup and file-search results are especially sensitive to OS binary
verification and filesystem caches. Compare repeated runs on your target workload
instead of treating small timing differences as guarantees.

## Platform and installation verification

The [release verification workflow](https://github.com/ErzenXz/codex-harness/actions/runs/39605798717)
passed on macOS, Linux and Windows, each on arm64 and x64. It runs Rust tests,
ACP integration tests, launcher and installation checks, then validates native
packages and standalone archives. Performance numbers above come from macOS;
the other platforms were checked for functionality.

A separate fresh pnpm installation from the public npm registry installed only
the matching macOS arm64 package. Its installed command passed 23 ACP integration
tests; the Linux X11 desktop fixture was skipped. All seven npm payloads matched
the verified release artifacts' SHA-512 integrity values.
