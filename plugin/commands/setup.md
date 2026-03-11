---
description: Run crustyclaw setup (Claude auth + Telegram bot configuration)
---

Run the crustyclaw setup wizard. Since Claude Code's terminal doesn't support interactive stdin, orchestrate the setup conversationally by collecting values from the user and passing them as CLI flags.

## Step 1: Check Claude authentication

```bash
claude auth status 2>&1 || true
```

If not authenticated, tell the user they need to run `claude auth login` in a separate terminal first (this requires a browser OAuth flow that can't be done inside Claude Code). Then re-check.

## Step 2: Collect Telegram bot token

Ask the user:
> To set up crustyclaw, I need your Telegram bot token. If you don't have one yet, create a bot via [@BotFather](https://t.me/BotFather) on Telegram and copy the token it gives you.

Wait for the user to provide the token.

## Step 3: Run setup with pairing flow

Once you have the token, run setup. This validates it and starts the pairing flow (prints a code for the user to send to the bot, then waits up to 60s):

```bash
"${CLAUDE_PLUGIN_ROOT}/bin/crustyclaw" setup --token "<TOKEN>" --yes
```

Show the user the pairing code from the output and tell them to send it to their bot on Telegram.

If the pairing times out, tell the user and re-run the same command to try again. Do NOT ask for a chat ID — pairing is the only supported method.

## Step 4: Done

After setup completes successfully, suggest running `/crustyclaw:start` to launch the daemon.
