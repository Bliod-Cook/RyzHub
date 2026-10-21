# RyzHub

[![CI](https://github.com/Bliod-Cook/RyzHub/actions/workflows/ci.yml/badge.svg)](https://github.com/Bliod-Cook/RyzHub/actions/workflows/ci.yml)

A self-hosted agent platform. Chat with models that run code in a sandbox, or hand a task to
Codex, Claude Code or any ACP agent working in a persistent, checkpointed workspace.
Rust on the server, one Flutter app for Windows, Android and the web.

## Features

- **Chat and Work.** *Chat* is the platform's own agent loop: streaming replies, sandboxed
  tools, web search, files handed back to you. *Work* runs a native harness —
  [Codex](https://github.com/openai/codex), Claude Code, or any
  [Agent Client Protocol](https://agentclientprotocol.com) agent — in a long-lived workspace
  with steering, approvals, forks and a terminal.
- **Model gateway.** Anthropic Messages, OpenAI Responses, Chat Completions and Gemini pass
  through unchanged, with per-model upstream failover, bring-your-own-key credentials,
  reasoning limits and usage metering.
- **Sandboxes.** containerd with runc or gVisor, no network beyond an allow-listed egress
  proxy, per-sandbox disk quotas, and workspaces checkpointed to a directory or S3.
- **Files, search and memory.** A personal file library with resumable uploads; hybrid
  full-text and vector search over messages and project files; explicit, undoable memories.
- **Plugins.** Remote MCP tools, skills and commands from a URL, git repository, upload or
  catalog — with signature checks, OAuth account linking, elicitation and MCP Apps.
- **Offline-tolerant clients.** A shared Rust core keeps the timeline, cache and offline
  command queue; Markdown, code highlighting and diffs are rendered in Rust.
- **Multi-tenant.** Organizations isolated by row-level security, live membership checks,
  OpenID Connect and GitHub sign-in, Android push notifications.

## Quick start

You need Rust 1.94+, Docker Compose and Python 3.

```sh
git clone https://github.com/Bliod-Cook/RyzHub.git
cd RyzHub
./scripts/dev.sh
```

This starts PostgreSQL and NATS in Docker, migrates the database and serves the API on
`http://127.0.0.1:8080`. Replies come from a local model fixture, so no API key is needed.

Then build and serve the web client in a second terminal (Flutter 3.47.5):

```sh
cargo install flutter_rust_bridge_codegen --version 2.11.1 --locked
cargo install cargo-expand wasm-pack
rustup toolchain install nightly-2026-09-25 --profile minimal --component rust-src --target wasm32-unknown-unknown
./scripts/build-web.sh
python3 scripts/serve-web.py
```

Open <http://127.0.0.1:3000> and register with any email and a password of 8+ characters.
Every new account gets a personal organization. To run an instance for real, see
[Self-hosting](#self-hosting).

## Development

### Local stacks

| Script | Runs | API |
|---|---|---|
| `scripts/dev.sh` | Server and a local model fixture; no sandboxes | `:8080` |
| `scripts/dev-fixture.sh` | Execution node, Codex and Claude Code harnesses, model fixtures | `:8280` |
| `scripts/dev-live.sh` | Like `dev-fixture.sh`, against the real providers in `var/dev/config.toml` and `var/dev/secrets.env` | from config |

The sandbox stacks need Linux and Docker: the execution node runs in a privileged container
named `ryzhub-execution`. They also need a static (musl) build of Codex 0.154.0 on `PATH` or
in `RYZHUB_CODEX_BINARY`; on first run `dev-fixture.sh` copies it into the node, downloads
gVisor and builds the harness image.

All stacks share the vault master key in `var/vault.env`, created on first run. Keep it:
stored credentials are encrypted with it.

### Clients

- **Web:** `./scripts/build-web.sh [flutter build args]`, then `python3 scripts/serve-web.py`
  (it sends the cross-origin isolation headers the Wasm core needs).
- **Windows:** `scripts/build-windows.ps1` on a machine with the Visual Studio C++ toolchain.
- **Android:** `scripts/build-android.sh` with the SDK/NDK, `cargo-ndk` and the Android Rust
  targets. Push notifications need `client/android/app/google-services.json`.

### Tests

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --locked
./scripts/test-client.sh        # Flutter analyze, widget and golden tests
./scripts/test-integration.sh   # consistency and failure recovery
./scripts/test-platform.sh      # Chat, model gateway, plugins, platform APIs
./scripts/test-work.sh          # Codex Work, checkpoints, S3 (MinIO)
./scripts/test-harnesses.sh     # Codex, Claude Code and OpenCode sessions
./scripts/test-search.sh        # search and memory
```

Integration suites run against real PostgreSQL, NATS and containerd in throwaway databases.
The sandbox suites (platform, work, harnesses) need the same Linux setup as `dev-fixture.sh`
and a musl C compiler (`CC_x86_64_unknown_linux_musl=gcc` works when `musl-gcc` is missing).
A few checks call a real model provider and are skipped unless `RYZHUB_TEST_PROVIDER_KEY`
plus `RYZHUB_TEST_EMBEDDING_URL`/`_MODEL` (search) or `RYZHUB_TEST_PROVIDER_URL`/`_MODEL`
(platform, Responses protocol) are set — the scripts read them from `.env`.

See [AGENTS.md](AGENTS.md) for coding conventions, generated files and commit style.

## Architecture

```text
 Flutter app (UI + Rust core)
        │  WebSocket · SSE · REST
        ▼
     gateway ──────────┐
                       ├──  PostgreSQL · NATS · blob storage
     owner ────────────┘
        │ Chat loop           │ sandboxes, checkpoints
        ▼                     ▼
   llm-gateway ◄─────── execution node ── sandbox (guest, tools, harness)
        │
        ▼
   model providers
```

One `ryzhub-server` binary plays every server role. The *gateway* serves clients; an *owner*
holds a fenced lease on each active conversation and is the only writer of its events; the
*llm-gateway* proxies model calls, including those made from sandboxes. Execution nodes run
sandboxes under containerd; a guest process inside each sandbox runs tools and harnesses.

| Path | Contents |
|---|---|
| `crates/server` | REST and WebSocket API, row-level-secured store, owner actor, Chat loop, harness projection, model and MCP gateways, plugins, workspaces |
| `crates/execution` | containerd host, guest journal, exec, Codex/Claude/ACP drivers, egress proxy, workspace archives |
| `crates/core` | Client state machine: sequencing, subscriptions, offline commands, timeline, Markdown, highlighting, diff |
| `crates/bridge`, `client/` | flutter_rust_bridge layer and the Flutter app |
| `crates/domain`, `crates/proto`, `proto/` | Authorization, policy, and the Protobuf wire schema |
| `harness/claude` | TypeScript adapter between the guest and the Claude Agent SDK |
| `migrations/`, `scripts/` | SQL migrations, dev and test scripts |
| `deploy/` | Self-hosting: images, Docker Compose, the PostgreSQL image (PGroonga, pgvector) |

## Self-hosting

[deploy/README.md](deploy/README.md) runs an instance on one Linux machine with Docker
Compose: `deploy/setup.sh ryz.example.com`, add a model provider key, and sign in. It also
covers configuration, upgrades, backups and running on more than one machine.

## Plugins

A plugin is an Agent Plugins 1.0.0 package: `plugin.json`,
`mcp.json` (streamable-HTTP servers) and `skills/<name>/SKILL.md`. Platform extensions live
under `extensions.app.ryzhub`:

- `permissions.deny` — tools never offered to the model;
- `commands` — prompt templates with JSON-schema parameters, shown in the `/` menu;
- `ui.mcpApps` — let tool results open MCP App pages;
- `userConfig` — settings and secrets, optionally sent to the plugin's servers as headers.

Packages are labelled *signed* (verified against `plugins.trusted_keys`), *reviewed* (from the
instance catalog with a matching hash) or *unverified*, which policy can refuse. Claude Code
plugins (`.claude-plugin/plugin.json`) are converted on install.
Tools reach the model as `<plugin>__<tool>`; tools that change after install are held back
until the member confirms them.

## Documentation

- [Self-hosting](deploy/README.md) — install, configure and run an instance
- [Wire protocol](docs/wire-protocol-v1.md) — client/server frames, channels and commands
- [TODO](docs/TODO.md) — what the design calls for that isn't built yet
- [Device checklist](docs/device-checklist.md) — manual checks on Windows and Android

The documents under `docs/` are written in Chinese.

## License

Licensed under either the MIT license or the Apache License 2.0, at your option.
