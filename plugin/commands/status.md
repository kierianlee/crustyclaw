---
description: Show the current crustyclaw daemon status
disable-model-invocation: true
---

Check the crustyclaw daemon status by running:

```bash
CC_BIN="${HOME}/.crustyclaw/bin/crustyclaw"; [ -x "$CC_BIN" ] || CC_BIN="${CLAUDE_PLUGIN_ROOT}/bin/crustyclaw"; "$CC_BIN" statusline
```

Report the daemon status to the user.
