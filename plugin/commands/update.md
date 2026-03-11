---
description: Update crustyclaw to the latest release
disable-model-invocation: true
---

Update the crustyclaw binary to the latest release by running:

```bash
CC_BIN="${HOME}/.crustyclaw/bin/crustyclaw"; [ -x "$CC_BIN" ] || CC_BIN="${CLAUDE_PLUGIN_ROOT}/bin/crustyclaw"; "$CC_BIN" update
```

Report the output. If the daemon was running, it will be stopped, updated, and restarted automatically.
