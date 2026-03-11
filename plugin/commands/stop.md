---
description: Stop the crustyclaw daemon gracefully
disable-model-invocation: true
---

Stop the crustyclaw daemon by sending SIGTERM to the running process:

```bash
pkill -TERM -f "crustyclaw$" 2>/dev/null || echo "No crustyclaw process found"
```

Wait 2 seconds, then verify it stopped:

```bash
CC_BIN="${HOME}/.crustyclaw/bin/crustyclaw"; [ -x "$CC_BIN" ] || CC_BIN="${CLAUDE_PLUGIN_ROOT}/bin/crustyclaw"; "$CC_BIN" statusline
```

Report whether the daemon stopped successfully.
