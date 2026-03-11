---
description: Start the crustyclaw daemon in the background
disable-model-invocation: true
---

Start the crustyclaw daemon by running:

```bash
CC_BIN="${HOME}/.crustyclaw/bin/crustyclaw"; [ -x "$CC_BIN" ] || CC_BIN="${CLAUDE_PLUGIN_ROOT}/bin/crustyclaw"; "$CC_BIN" start
```

Report the output. If it fails, check `/tmp/crustyclaw.log` for errors.
