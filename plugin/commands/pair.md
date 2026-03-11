---
description: Pair a new Telegram user with crustyclaw (the daemon must be stopped first)
disable-model-invocation: true
---

Run the interactive pairing flow to add a new Telegram user. The daemon must be stopped first since both use the Telegram getUpdates API:

```bash
CC_BIN="${HOME}/.crustyclaw/bin/crustyclaw"; [ -x "$CC_BIN" ] || CC_BIN="${CLAUDE_PLUGIN_ROOT}/bin/crustyclaw"; "$CC_BIN" pair
```

This is an interactive process — let the user interact with the terminal directly. Do not try to automate the prompts.
