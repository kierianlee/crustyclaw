<div align="center">

# crustyclaw

**Turn Claude Code into a remote AI agent you can control from anywhere.**

A lightweight Rust daemon that bridges Claude Code with Telegram — giving you remote command execution, scheduled automation, health monitoring, and interactive tool approval, all from your phone.

[Getting Started](#getting-started) · [Features](#features) · [Configuration](#configuration)

</div>

---

## Why crustyclaw?

Claude Code is powerful, but it's bound to your terminal. **crustyclaw** frees it — run prompts, schedule recurring jobs, and approve tool calls from Telegram while you're away from your desk.

- **Message Claude from Telegram** — send text, photos, or voice notes and get responses back
- **Schedule jobs with cron** — recurring prompts, reminders, or automated workflows
- **Approve tool calls remotely** — inline Telegram buttons for Allow / Always Allow / Deny
- **Heartbeat monitoring** — periodic health checks with alerts when something needs attention
- **Session persistence** — conversations survive daemon restarts
- **Voice transcription** — send voice notes, get them transcribed and processed via whisper

## How Is This Different from Claude Code on Mobile?

Claude Code's mobile app runs in a **cloud sandbox**. crustyclaw runs Claude Code on **your local machine** — your real files, your installed tools, your SDKs, your MCP servers.

It also adds capabilities Claude Code doesn't have out of the box:

- **Cron-based job scheduling** with persistence across restarts
- **Heartbeat monitoring** with Telegram alerts
- **Soul prompts** for persistent personality and context
- **Voice note transcription** via whisper
- **Session auto-resume** across daemon restarts
- **Multi-user access** via Telegram pairing

## Features

### Remote Claude Invocation

Send messages to your Telegram bot and crustyclaw forwards them to Claude Code. Responses stream back in real-time with automatic chunking for long replies. Attach photos or voice notes — they're downloaded, processed, and included in the prompt.

### Job Scheduling

Schedule recurring or one-shot jobs using cron expressions. Jobs persist across daemon restarts.

| Action | Description |
|--------|-------------|
| `ClaudePrompt` | Run a prompt through Claude and send the response to a chat |
| `TelegramMessage` | Send a static message to any chat |
| `TelegramAdmin` | Send a message to the admin |

### Tool Approval via Telegram

When Claude wants to run a tool (write a file, execute a command, etc.), crustyclaw sends you a formatted preview on Telegram with inline buttons:

- **Allow** — approve this single invocation
- **Always Allow** — auto-approve this tool for the rest of the session
- **Deny** — block the tool call

It's the same security model as sitting at your terminal — permission prompts are just routed to Telegram instead. Uses Unix domain sockets and Claude Code's `PreToolUse` hook system, with a configurable timeout and automatic denial.

### Heartbeat Monitoring

Periodic health checks that invoke Claude with a configurable prompt. If the response isn't `HEARTBEAT_OK`, crustyclaw alerts you via Telegram. Useful for monitoring background agents or detecting stale sessions.

### Voice Transcription

Send voice notes via Telegram and crustyclaw transcribes them using `whisper-cpp` before forwarding to Claude. Requires `ffmpeg` and `whisper-cpp` in PATH.

## Getting Started

### Prerequisites

- [Claude Code](https://docs.anthropic.com/en/docs/claude-code) CLI installed and authenticated
- A [Telegram bot token](https://core.telegram.org/bots#how-do-i-create-a-bot) (from @BotFather)

There are **3 ways** to run crustyclaw:

1. [Claude Code Plugin](#option-1-claude-code-plugin-recommended) (recommended)
2. [Standalone Daemon](#option-2-standalone-daemon)
3. [Docker](#option-3-docker)

---

### Option 1: Claude Code Plugin (Recommended)

The plugin auto-starts the daemon when you open a Claude Code session and stops it when you close. No manual intervention needed.

> **Note:** Official plugin marketplace submission is in progress. In the meantime, clone the repo and build locally. You'll need [Rust 1.89+](https://rustup.rs/) installed.

```bash
git clone https://github.com/kierianlee/crustyclaw.git
cd crustyclaw

# Build the binary and copy it into the plugin directory
./plugin/scripts/build-plugin.sh

# Launch Claude Code with the plugin loaded
claude --plugin-dir ./plugin
```

Once inside Claude Code, run `/crustyclaw:setup` to configure your Telegram bot and pair your account. That's it — the daemon starts automatically from now on.

#### Plugin Commands

| Command | Description |
|---------|-------------|
| `/crustyclaw:setup` | Run the interactive setup wizard |
| `/crustyclaw:start` | Start the daemon in the background |
| `/crustyclaw:stop` | Gracefully stop the daemon |
| `/crustyclaw:status` | Show daemon status |
| `/crustyclaw:pair` | Pair a new Telegram user |
| `/crustyclaw:doctor` | Diagnose common issues (9 checks) |

---

### Option 2: Standalone Daemon

Run crustyclaw as a standalone daemon, independent of any Claude Code session. Requires [Rust 1.89+](https://rustup.rs/).

```bash
git clone https://github.com/kierianlee/crustyclaw.git
cd crustyclaw
cargo build --release
```

The binary is at `target/release/crustyclaw`.

**First-time setup:**

```bash
./target/release/crustyclaw setup
```

This will:
1. Verify Claude CLI authentication (or prompt you to log in)
2. Ask for your Telegram bot token
3. Generate a pairing code — send it to your bot to link your Telegram account
4. Write the config to `~/.crustyclaw/config.json`

**Start the daemon:**

```bash
# Foreground (with logs to stdout)
./target/release/crustyclaw

# Background (logs to /tmp/crustyclaw.log)
./target/release/crustyclaw start
```

---

### Option 3: Docker

Run crustyclaw in a container. You still need to run setup on the host first.

**Prerequisites:**
1. Claude CLI authenticated on the host (`~/.claude/` with valid OAuth tokens)
2. crustyclaw configured on the host (`~/.crustyclaw/config.json` — run `crustyclaw setup` first, or use Option 1)

```bash
docker compose up -d
```

The `docker-compose.yml` mounts both directories into the container:

```yaml
volumes:
  - ${HOME}/.claude:/home/crustyclaw/.claude:rw      # OAuth tokens
  - ${HOME}/.crustyclaw:/home/crustyclaw/.crustyclaw:rw  # Config & data
```

Override settings via a `.env` file:

```bash
CRUSTYCLAW_MODEL=opus
CRUSTYCLAW_HEARTBEAT_INTERVAL=600
```

The image is a multi-stage build: Rust 1.89 builder + Node.js 22 slim runtime with Claude Code CLI, ffmpeg, and a non-root user.

---

### Adding a User

To pair additional Telegram users (works with any method):

```bash
# Stop the daemon first (both use Telegram's getUpdates API)
crustyclaw pair
```

This generates a pairing code. The new user sends it to the bot, and they're added to the allowed users list.

## Configuration

Config lives at `~/.crustyclaw/config.json` (created by `setup`). All fields can be overridden with environment variables using the `CRUSTYCLAW_` prefix.

### Key Options

| Field | Default | Env Override | Description |
|-------|---------|--------------|-------------|
| `model` | `"sonnet"` | `CRUSTYCLAW_MODEL` | Claude model to use |
| `fallback_model` | — | `CRUSTYCLAW_FALLBACK_MODEL` | Fallback model on rate limits |
| `permission_mode` | `"accept_edits"` | `CRUSTYCLAW_PERMISSION_MODE` | `dangerously_skip`, `accept_edits`, or `interactive` |
| `subprocess_timeout_secs` | `300` | `CRUSTYCLAW_TIMEOUT` | Max time per Claude invocation |
| `heartbeat_enabled` | `true` | `CRUSTYCLAW_HEARTBEAT_ENABLED` | Enable periodic health checks |
| `heartbeat_interval_secs` | `900` | `CRUSTYCLAW_HEARTBEAT_INTERVAL` | Seconds between heartbeats |
| `voice_enabled` | `false` | `CRUSTYCLAW_VOICE_ENABLED` | Enable voice transcription |
| `telegram_approval` | `true` | — | Enable tool approval via Telegram |
| `approval_timeout_secs` | — | `CRUSTYCLAW_APPROVAL_TIMEOUT` | Seconds to wait for approval |
| `max_budget_usd` | — | `CRUSTYCLAW_MAX_BUDGET` | Per-invocation budget cap |
| `working_dir` | — | `CRUSTYCLAW_WORKING_DIR` | Working directory for Claude |
| `allowed_tools` | — | — | Whitelist of tool names |
| `disallowed_tools` | — | — | Blacklist of tool names |

### Soul Prompts

Drop `.md` files in `~/.crustyclaw/prompts/` to inject persistent context into every Claude invocation. Useful for personality, project context, or standing instructions. Files are loaded alphabetically, capped at 64 KiB total.

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│                     crustyclaw daemon                   │
│                                                         │
│  ┌──────────┐    ┌────────────┐    ┌─────────────────┐  │
│  │ Telegram  │───>│  Request   │───>│  Claude Code    │ │
│  │ Poll Loop │    │  Queue     │    │  CLI Invocation │ │
│  └──────────┘    └────────────┘    └────────┬────────┘  │
│                       ^                      │          │
│  ┌──────────┐         │                      v          │
│  │ Scheduler │────────┘         ┌─────────────────────┐ │
│  │ (cron)    │                  │  Session Manager    │ │
│  └──────────┘                   │  (persist/resume)   │ │
│                                 └─────────────────────┘ │
│  ┌──────────┐    ┌────────────────────────────────────┐ │
│  │ Heartbeat │───>│  Status Tracker (status.json)     │ │
│  └──────────┘    └────────────────────────────────────┘ │
│                                                         │
│  ┌──────────────────────────────────────────────────┐   │
│  │  Permission Server (Unix socket)                 │   │
│  │  PreToolUse hook <──> Telegram inline buttons    │   │
│  └──────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
```

### Data Directory

```
~/.crustyclaw/
├── config.json       # Main configuration
├── session.json      # Claude session state
├── scheduler.json    # Persisted cron jobs
├── status.json       # Runtime status (flushed every 5s)
├── daemon.lock       # Exclusive lock file
├── permission.sock   # Unix socket for tool approval
├── prompts/          # Soul prompts (*.md)
└── inbox/            # Downloaded photos & voice notes
```

## CLI Reference

```
crustyclaw              Start daemon (foreground)
crustyclaw start        Start daemon (background, logs to /tmp/crustyclaw.log)
crustyclaw setup        Interactive first-time setup
crustyclaw pair         Pair a new Telegram user
crustyclaw statusline   Print daemon status (for status bar integration)
crustyclaw help         Show usage
```

## Resilience

- **Rate limits** — automatic retry with jitter
- **Session expiry** — reset and retry transparently
- **Connection failures** — exponential backoff (up to 120s) for Telegram
- **Corrupted state** — graceful fallback (new session, skip invalid jobs)
- **Panic isolation** — each request runs in its own task
- **Atomic file writes** — write-tmp-fsync-rename pattern prevents corruption
- **Exclusive locking** — only one daemon instance can run at a time

## License

MIT