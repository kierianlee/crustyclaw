---
description: Stop the crustyclaw daemon gracefully
disable-model-invocation: true
---

Stop the crustyclaw daemon gracefully and report the output:

```bash
CC_BIN="${HOME}/.crustyclaw/bin/crustyclaw"; [ -x "$CC_BIN" ] || CC_BIN="${CLAUDE_PLUGIN_ROOT}/bin/crustyclaw"; "$CC_BIN" stop
```
