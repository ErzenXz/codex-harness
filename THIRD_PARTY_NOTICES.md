# Upstream contracts

This is an independent implementation inspired by documented agentic behavior,
not a copy of proprietary Claude Code internals.

- [Agent Client Protocol](https://agentclientprotocol.com/protocol/overview)
- [Claude Code agent loop and subagents](https://code.claude.com/docs/en/how-claude-code-works)
- [OpenAI Responses](https://platform.openai.com/docs/api-reference/responses)
- [OpenAI Codex authentication](https://developers.openai.com/codex/auth)
- [Open-source Codex device flow](https://github.com/openai/codex/blob/main/codex-rs/login/src/device_code_auth.rs)
- [Open-source Codex OAuth client](https://github.com/openai/codex/blob/main/codex-rs/login/src/auth.rs)
- [OpenAI prompt caching](https://platform.openai.com/docs/guides/prompt-caching)

The ChatGPT adapter uses the public client identifier and endpoints used by the
open-source Codex client. Those values are not secrets, do not grant account
access, and are not a guarantee that third-party use remains supported. The user
must explicitly authenticate with an eligible account. OpenAI controls access,
rate limits, caching, model availability, and backend compatibility.

Dependencies retain their respective licenses; see Cargo.lock for exact versions.
