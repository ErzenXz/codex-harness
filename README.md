# Codex Harness

A fast, quiet Rust backend for coding agents. Your **ACP-compatible app provides
the UI**; this runs behind it. Includes ChatGPT subscription login, file/search
tools, subagents, background jobs, MCP servers and optional computer use.

## 1. Install

You need **Git** and **[Rust 1.88+](https://rustup.rs/)**. Commands below use a
Linux/macOS shell; on Windows, use WSL. No Node.js or Python needed to run it.
[ripgrep](https://github.com/BurntSushi/ripgrep#installation) is recommended for
fast search, but optional: a Rust search fallback is built in.

```sh
git clone https://github.com/ErzenXz/codex-harness.git
cd codex-harness
cargo install --path . --locked
codex-harness --version
```

This builds from source and installs the executable into Cargo's bin directory
(normally `~/.cargo/bin`). If the command is not found, restart your terminal or
run `export PATH="$HOME/.cargo/bin:$PATH"`.

## 2. Sign in with ChatGPT

While still in the cloned repo, copy the starter config **once** to your user
config directory, outside any project an agent will edit:

```sh
mkdir -p "$HOME/.config/codex-harness"
cp -n examples/chatgpt.toml "$HOME/.config/codex-harness/chatgpt.toml"
codex-harness login --config "$HOME/.config/codex-harness/chatgpt.toml"
```

Open the link printed by `login`, enter its code, and approve access. If requested,
enable device-code login in your ChatGPT security settings. Your plan must allow
the model; subscription usage limits still apply.

The starter config uses **`gpt-6-luna`, low reasoning, and at most 10 simultaneous
model requests/subagents**. This is a concurrency cap, not a total usage budget.
No API key is needed for subscription login.

## 3. Run from your ACP app

Tell your ACP app to launch `codex-harness` with these arguments. This is a generic
launcher definition; the settings location/outer format depends on your app:

```json
{
  "command": "/home/YOU/.cargo/bin/codex-harness",
  "args": [
    "--acp",
    "--config",
    "/home/YOU/.config/codex-harness/chatgpt.toml"
  ],
  "env": {}
}
```

**Replace both example paths with your real absolute paths.** Find them with:

```sh
command -v codex-harness
printf '%s\n' "$HOME/.config/codex-harness/chatgpt.toml"
```

Your app starts the process and sends prompts/approvals over stdin/stdout. It can
open sessions for any project directory; no harness files need to go in that
project. Keep the config in your user directory and always pass `--config`.

To start it yourself instead:

```sh
codex-harness --acp --config "$HOME/.config/codex-harness/chatgpt.toml"
```

**A blank terminal is normal.** There is no terminal chat, web server or status
banner. Without an ACP client, it just waits for protocol messages. Press
**Ctrl+C** to stop; don't type chat messages into the terminal or use `&` as a
replacement for an ACP launcher.

## Other providers and optional tools

| Want to… | Use |
| --- | --- |
| Use an OpenAI API key | [API-key setup](docs/reference.md#openai-api-key) |
| Use OpenRouter | [Starter config](examples/openrouter.toml); supply `OPENROUTER_API_KEY` in your app's launcher environment |
| Use a local Ollama model | [Starter config](examples/ollama.toml); install Ollama and pull the model separately |
| Add MCP tools | [MCP setup](docs/reference.md#mcp-servers); your ACP client supplies `mcpServers` when creating/loading a session |
| Enable computer use | [Computer setup](docs/reference.md#opt-in-computer-use); native driver requires Linux X11, xdotool and ImageMagick |

Copy a different starter config into your user config directory and point
`--config` at it. MCP and computer use are optional; neither starts extra
processes until needed, and native computer use is off by default.

**Permissions matter:** writes, edits, shell commands, MCP calls and desktop
actions need approval from your ACP client. Approved shell/MCP/desktop access is
**not an OS sandbox**. Login tokens are stored unencrypted locally; screenshots
are shared with the model provider. [Security details](docs/reference.md#safety-boundaries).

## Update or sign out

Update from inside your clone:

```sh
git pull --ff-only
cargo install --path . --locked --force
```

Restart your ACP app's agent process after updating. Your user config is kept.
To remove saved login credentials, run the following and stop/restart any running
harness processes (they cache credentials):

```sh
codex-harness logout --config "$HOME/.config/codex-harness/chatgpt.toml"
```

## Troubleshooting

- **`cargo` missing / Rust too old:** install or update Rust using [rustup](https://rustup.rs/), then reopen the terminal.
- **Linker missing:** install your OS build tools (Ubuntu/Debian: `build-essential`; macOS: Xcode Command Line Tools).
- **Installed, but nothing appears:** expected—connect an ACP client as in step 3.
- **It immediately exits:** check stderr/config paths; closed stdin also shuts it down.
- **Login/model access fails:** check device-code login, your plan's model access and remaining allowance. There is no automatic model fallback.
- **Wrong provider/model:** pass the intended `--config`; `OPENAI_MODEL` and `OPENAI_BASE_URL` override config values. Avoid stale launcher environment settings.

[Technical reference](docs/reference.md) · [Development & tests](docs/reference.md#development) · [Performance & limits](docs/reference.md#performance-and-memory)
