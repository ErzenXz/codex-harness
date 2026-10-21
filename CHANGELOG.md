# 0.3.0

- Discover available text models using your existing ChatGPT login or configured
  OpenAI-compatible provider, with names and model-specific reasoning levels.
- Standard ACP model, reasoning and Code/Plan selectors; legacy selection still
  works. Persist choices per session and pass them through to model requests and
  subagents. Reject unsupported values without changing the session.
- Share one lazy catalog fetch across concurrent sessions and cache it privately
  for five minutes. Discovery is bounded to five seconds, 2 MiB input, 256 KiB normalized metadata and 512 models;
  unavailable providers fall back to the configured model. No new dependencies.
- Add per-model reasoning metadata for CodeInk's model picker, and protect OAuth
  token rotation when a discovery deadline cancels its waiting request.

# 0.2.1

- Fix CodeInk and other ACP clients selecting the advertised model before a
  prompt. `session/set_model` now accepts repeated selection of the session's
  configured model instead of returning "Method not found".
- Reject unavailable models without changing the session; cover selection,
  consecutive prompts and session reload with an ACP regression test.

# 0.2.0

- One-command npm/pnpm installation with only the matching native Rust binary.
  Six native targets: macOS/Linux/Windows, arm64/x64. No installation scripts.
- Smaller release binary; metadata commands avoid starting the async runtime.
- Stream tool arguments in place, parse SSE by fragments, count JSON without
  allocating a duplicate buffer, and trim persisted history without cloning it.
- Reuse search buffers and bound session reads even if a file grows during a read.
- Fix canonical path protection on macOS and Windows, including private-state
  search exclusion and NTFS alternate-stream credential protection.
- Native Windows state paths, environment essentials, shell quoting and Job Object
  process cleanup. Approved shell access remains unsandboxed.
- Native tests and release smoke checks for every supported platform/architecture;
  exact npm version pins, package allowlists, checksummed standalone archives.
