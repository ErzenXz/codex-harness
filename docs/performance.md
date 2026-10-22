# Performance

Piko is a single native Rust process with one async event-loop thread,
shared HTTP connections and on-demand blocking workers. It starts no MCP servers,
browsers or models until explicitly needed. Cap concurrency to your machine and
provider allowance; unload idle sessions with `_harness/session/unload` to release
retained history and jobs.

Version 0.3 adds lazy model discovery using the existing HTTP pool. Sessions share
one immutable catalog; in-memory and private disk caches last five minutes.
Discovery has a five-second deadline, a 2 MiB input limit and a 256 KiB normalized
metadata limit. It adds no runtime dependencies or background polling.

Version 0.2 removes repeated streamed-argument copies, appends SSE line fragments
in bulk, moves completed output, counts JSON without duplicate buffers and trims
the old rolling history without cloning the result. Version 0.5 preserves durable
history separately from its model-context projection. Built-in grep reuses one bounded file buffer.
Version 0.5 uses size optimization (`z`), full LTO and one codegen unit.
`--help` and `--version` return before creating an async runtime or user state.

The npm launcher installs one exact-version platform binary, with no postinstall
script or network access at launch. On macOS/Linux it calls Node's `execve` when
available, replacing Node with Rust and freeing the launcher memory. On Windows
Node remains as a forwarding process. Use the path from `--print-binary-path`
directly in your ACP client to eliminate launcher overhead on every platform.

Version 0.5 borrows Responses input during serialization, encodes it once and
shares request bytes through retries. Those bytes are freed after successful
headers, before waiting for the response stream. Durable active-turn checkpoints
encode only current work; atomic file operations run in one blocking dispatch.
Extra durability checks have a cost, particularly for very short mock responses.

## Reproduce

```sh
cargo build --release --locked
PIKO_BIN=target/release/piko python3 tests/benchmark.py --agents 10
```

On Windows set `PIKO_BIN` to the `.exe` path in your shell first. The benchmark
uses temporary state/workspaces and loopback mock SSE; no credentials or paid
model calls. It reports native binary bytes, ACP startup, ten simultaneous turns,
process RSS (Linux/macOS) and search over 10,000 files. The default is 256 turns;
`--agents 10` selects the smaller fixture. Startup includes fixture
setup; first-launch OS verification and filesystem cache materially affect it.

Results do not establish a competitor ranking, model intelligence, real provider
throughput or a cache hit rate. RSS varies with platform, workload, active tools,
conversation length and configured concurrency. Context bytes bound serialized
model input; durable history has a separate 16 MiB/session cap. Neither caps total
process RSS. Read the runtime limits in the
[technical reference](reference.md#performance-and-memory).

## Version 0.6.1 tools and footprint (2026-10-22)

Compared the installed 0.5.0 npm binary against a local 0.6.1 release build on the
same macOS arm64 machine. The earlier 0.5.0 baseline uses the same fixture; three
0.6.1 runs were recorded as a block after a dedicated warm-up, with no concurrent
local agent builds, tests or browser automation. These runs were not randomized.
Numbers below are medians. Startup includes fixture setup and excludes initial OS
verification; timing and RSS remain sensitive to filesystem caches and paging.

| Measurement | 0.5.0 | 0.6.1 |
| --- | ---: | ---: |
| Native binary bytes | 3,051,760 | 3,302,608 |
| ACP startup + initialize | 7.07 ms | 5.83 ms |
| Startup observed range | 6.97–12.34 ms | 5.82–5.96 ms |
| Idle sampled RSS | 3,776 KiB | 3,888 KiB |
| 256 mock prompts, 50 ms simulated delay | 0.650 s | 0.735 s |
| Sampled RSS after 256 completed prompts | 33,888 KiB | 30,048 KiB |
| 10,000-file search, end-to-end | 278.48 ms | 230.98 ms |

The new web, image, patch, PTY and messaging tools add **250,848 bytes (8.2%)** to
the native executable. This release is larger than 0.5.0. It ships no browser,
model, Node tool server or JavaScript runtime dependencies. Browser RAM is
external; each live terminal adds two small-stack I/O threads and OS resources.

Built-in tool schemas are shared across sessions instead of being rebuilt and
copied for every prompt. Responses reuses their encoded bytes across steps. This
removed an allocation regression measured during development. The final fixture's
completed-prompt RSS is lower, while idle RSS is slightly higher. Startup and
search are faster in these samples; the short-turn fixture is slower. This is
**not a general speedup or universal memory-reduction claim**. The expanded tool
catalog also consumes more model context and request bytes.

A separate held-stream fixture consumed every 128 KiB prompt and received all
successful SSE headers before sampling. At 64 concurrent streams, steady sampled
RSS was 47,840 → 46,352 KiB, and observed workload maximum was 53,280 → 51,296 KiB.
The 32-stream result is close (27,040 → 26,400 KiB); one stream is slightly higher
in 0.6.1 (4,960 → 5,104 KiB). Raw phase samples include page reclamation and all runs.

[Raw results, hashes, held-stream samples and validation scope](benchmarks/macos-arm64-0.6.1.json).
A real Chrome smoke test verified a local page through CDP navigation, snapshot,
typing, click, evaluation, screenshot and close with a temporary dedicated profile.
Public web fetching succeeded against agents.md; search uses documented HTTP
fixtures and no paid search key. A live ChatGPT coding check exercised image
viewing, patching and shell verification while preserving other fixture files.
The unpublished 0.6.0 candidate's [raw measurements](benchmarks/macos-arm64-0.6.0.json)
are retained separately; Windows startup negotiation was fixed before publishing.

## Version 0.5.0 comparison (2026-10-21)

Compared the installed 0.4.1 native binary against a local 0.5.0 release build,
on the same macOS arm64 machine with Rust 1.99. Three alternating runs per binary
used the identical 256-prompt fixture. Both binaries were warmed before these runs;
first-launch OS verification is excluded. Numbers below are medians of those runs.

| Measurement | 0.4.1 | 0.5.0 |
| --- | ---: | ---: |
| Native binary bytes | 4,561,504 | 3,051,760 |
| ACP startup + initialize | 13.36 ms | 8.08 ms |
| Idle sampled RSS | 4,064 KiB | 3,744 KiB |
| 256 mock prompts, 50 ms simulated model delay | 0.621 s | 0.644 s |
| 10,000-file search, end-to-end | 235.73 ms | 254.66 ms |

The binary is **33.1% smaller**. The short-turn timing is roughly flat, not an
end-to-end speedup claim. Search turns now include extra recovery checkpoints;
that durability adds work. Three runs do not establish timing significance.
Startup remains sensitive to filesystem and OS verification caches.

A separate allocation fixture fully consumed each 128 KiB request, sent successful
SSE headers, and held all response bodies. At **64 concurrent streams**, steady
sampled native RSS was **60,928 → 48,000 KiB (21.2% lower)**; the observed workload
peak was **66,256 → 52,960 KiB (20.1% lower)**. Both held phases were stable across
12 samples. These are process samples, not a guaranteed peak-memory bound.

The one- and 32-stream runs are included in the raw data too. Some phases show
page reclamation during sampling; at one stream the old binary's held-phase
median was lower. Small RSS differences and transient drops are not reliable
universal savings. This fixture excludes the Python server and npm launcher.

```sh
python3 tests/memory_benchmark.py --binary /absolute/old/piko \
  --binary /absolute/new/piko --agents 1 32 64 --prompt-kib 128
```

[Raw samples, binary hashes, environment and live coding-task result](benchmarks/macos-arm64-0.5.0.json).
The live task passed its independent tests and preserved an unrelated file;
it is one functional check, not an intelligence benchmark.

## Version 0.4.0 measurement (2026-10-21)

The [six-target Piko verification](https://github.com/ErzenXz/piko/actions/runs/39651456713)
passed on macOS/Linux/Windows, arm64/x64. The installed macOS arm64 binary matched
that CI artifact exactly: 4,561,504 bytes, with 3.97 MiB idle sampled RSS and
16.72 ms through ACP initialization in one local run.

This run used 256 simultaneous mock turns, completing in 0.793 seconds with a
50 ms simulated provider delay. Sampled RSS after those turns was 38.45 MiB.
The concurrency differs from the ten-turn fixtures below, so these are not direct
speed or memory comparisons. Model inference and real provider limits are excluded.

[Raw results and binary hash](benchmarks/macos-arm64-0.4.0.json).

## Version 0.3.1 measurement (2026-10-21)

The [six-target release verification](https://github.com/ErzenXz/piko/actions/runs/39641540994)
passed on native macOS/Linux/Windows runners, each on arm64 and x64. The public
npm install matched the verified macOS arm64 artifact by SHA-256.

One local run of the optimized macOS arm64 release measured a 4,576,720-byte
binary, 8.18 ms through ACP initialization, 4.02 MiB idle sampled RSS and 6.53 MiB
after ten concurrent mock turns. Those turns completed in 72 ms with 50 ms mock
provider delay. Initialization includes temporary fixture and HTTP setup.
These are synthetic overhead measurements, not provider throughput guarantees.

[Raw results and binary hash](benchmarks/macos-arm64-0.3.1.json).

## Version 0.2.0 before/after measurements (2026-10-21)

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

The [release verification workflow](https://github.com/ErzenXz/piko/actions/runs/39605798717)
passed on macOS, Linux and Windows, each on arm64 and x64. It runs Rust tests,
ACP integration tests, launcher and installation checks, then validates native
packages and standalone archives. Performance numbers above come from macOS;
the other platforms were checked for functionality.

A separate fresh pnpm installation from the public npm registry installed only
the matching macOS arm64 package. Its installed command passed 23 ACP integration
tests; the Linux X11 desktop fixture was skipped. All seven npm payloads matched
the verified release artifacts' SHA-512 integrity values.
