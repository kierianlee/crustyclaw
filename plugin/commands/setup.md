---
description: Run the interactive crustyclaw setup wizard (Claude auth + Telegram bot configuration)
disable-model-invocation: true
---

Run the interactive crustyclaw setup wizard. This requires user interaction (entering a Telegram bot token, sending a pairing code, etc.) so it must run in the foreground with stdin attached:

```bash
"${CLAUDE_PLUGIN_ROOT}/bin/crustyclaw" setup
```

This is an interactive process — let the user interact with the terminal directly. Do not try to automate the prompts. After setup completes, suggest running `/crustyclaw:start` to launch the daemon.
