"""Reproducible local-overhead benchmark, NOT a model-intelligence/speed benchmark."""
import argparse
import asyncio
import json
import hashlib
import platform
import os
from pathlib import Path
import statistics
import subprocess
import time

os.environ.setdefault("PIKO_BIN", os.environ.get("HARNESS_BIN", "target/release/piko"))
from stdio import BINARY, Harness, call, final, response


def memory(pid):
    status = Path(f"/proc/{pid}/status")
    if not status.exists():
        if platform.system() == "Darwin":
            result = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True, check=True)
            return {"VmRSS": int(result.stdout.strip())}
        return None
    values = {}
    for line in status.read_text().splitlines():
        key, _, value = line.partition(":")
        if key in ("VmRSS", "VmHWM", "Threads"):
            values[key] = int(value.split()[0])
    return values


async def benchmark(agents, delay):
    async def model(body):
        if body["input"][0].get("content", "").strip() == "SEARCH":
            if any(item.get("type") == "function_call_output" for item in body["input"]):
                return final()
            return response([call("grep", {"pattern": "UNIQUE_BENCHMARK_MARKER", "glob": "**/*.rs"})])
        await asyncio.sleep(delay)
        return final()

    started = time.perf_counter()
    async with Harness(model, extra=f"max_sessions = {max(agents, 256)}\nmax_model_requests = {agents}\n") as h:
        await h.initialize()
        startup_ms = (time.perf_counter() - started) * 1000
        idle_memory = memory(h.process.pid)
        started = time.perf_counter()
        sessions = await asyncio.gather(*(h.session() for _ in range(agents)))
        create_seconds = time.perf_counter() - started
        loaded_memory = memory(h.process.pid)
        started = time.perf_counter()
        latencies = []

        async def prompt(session):
            start = time.perf_counter()
            result = await h.prompt(session)
            assert result.get("result", {}).get("stopReason") == "end_turn", result
            latencies.append((time.perf_counter() - start) * 1000)

        await asyncio.gather(*(prompt(session) for session in sessions))
        seconds = time.perf_counter() - started
        concurrent_memory = memory(h.process.pid)
        model_peak = h.peak_active
        # Search timings include two loopback model round-trips, ACP, and persistence.
        source = h.workspace / "src"
        source.mkdir()
        for index in range(10000):
            (source / f"file-{index}.rs").write_text("fn main() {}\n" + ("// UNIQUE_BENCHMARK_MARKER\n" if index == 7777 else ""))
        search_ms, engines = [], set()
        for _ in range(5):
            await h.rpc("_harness/session/unload", {"sessionId": sessions.pop()})
            session = await h.session()
            start = time.perf_counter()
            result = await h.prompt(session, "SEARCH")
            assert result.get("result", {}).get("stopReason") == "end_turn", result
            search_ms.append((time.perf_counter() - start) * 1000)
            output = json.loads(h.requests[-1]["input"][-1]["output"])
            assert "file-7777.rs" in output["content"], output
            engines.add(output["engine"])
            sessions.append(session)

        ordered = sorted(latencies)
        return {
            "scope": "Synthetic loopback SSE; small prompts; excludes model inference and provider limits. NOT a real cache-rate or competitor benchmark.",
            "binary_bytes": BINARY.stat().st_size,
            "binary_sha256": hashlib.sha256(BINARY.read_bytes()).hexdigest(),
            "environment": {"platform": platform.platform(), "logical_cpus": os.cpu_count()},
            "agents": agents, "simulated_model_delay_ms": delay * 1000,
            "startup_and_initialize_ms": round(startup_ms, 2),
            "create_sessions_seconds": round(create_seconds, 3),
            "complete_prompts_seconds": round(seconds, 3),
            "synthetic_prompts_per_second": round(agents / seconds, 1),
            "prompt_latency_ms": {"p50": round(statistics.median(ordered), 2), "p95": round(ordered[min(len(ordered)-1, int(len(ordered)*.95))], 2)},
            "peak_simultaneous_mock_requests": model_peak,
            "process_memory_kib_and_threads": {"idle": idle_memory, "loaded": loaded_memory, "after_prompts": concurrent_memory},
            "search_10000_files_end_to_end_ms": {"samples": [round(t, 2) for t in search_ms], "median": round(statistics.median(search_ms), 2), "engines": sorted(engines)},
        }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--agents", type=int, default=256, choices=range(5, 1025), metavar="5..1024")
    parser.add_argument("--model-delay", type=float, default=0.05)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    result = asyncio.run(benchmark(args.agents, args.model_delay))
    text = json.dumps(result, indent=2)
    print(text)
    if args.output:
        args.output.write_text(text + "\n")
