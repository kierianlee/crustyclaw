---
description: Diagnose and fix common crustyclaw issues
disable-model-invocation: true
---

Run the crustyclaw doctor to check for common issues. Execute each check below and report results:

## 1. Binary exists and is executable
```bash
CC_BIN="${HOME}/.crustyclaw/bin/crustyclaw"
if [ -x "$CC_BIN" ]; then
    echo "OK: binary found at $CC_BIN"
elif [ -x "${CLAUDE_PLUGIN_ROOT}/bin/crustyclaw" ]; then
    CC_BIN="${CLAUDE_PLUGIN_ROOT}/bin/crustyclaw"
    echo "OK: binary found at $CC_BIN (stable path missing — run /crustyclaw:update to fix)"
else
    echo "FAIL: binary not found — run /crustyclaw:setup to install"
fi
```

## 2. Claude CLI available
```bash
which claude >/dev/null 2>&1 && echo "OK: claude found at $(which claude)" || echo "FAIL: claude not in PATH"
```

## 3. Claude auth status
```bash
env -u CLAUDECODE claude auth status 2>&1 || echo "FAIL: claude auth check failed"
```

## 4. Config exists
```bash
DATA_DIR="${CRUSTYCLAW_DATA_DIR:-$HOME/.crustyclaw}"
test -f "${DATA_DIR}/config.json" && echo "OK: config found at ${DATA_DIR}/config.json" || echo "FAIL: no config — run /crustyclaw:setup"
```

## 5. Stale session file
Check if session.json references a session that might be expired. If the session has had repeated failures (invocation_count > 0 but recent errors), offer to reset it:
```bash
DATA_DIR="${CRUSTYCLAW_DATA_DIR:-$HOME/.crustyclaw}"
if [ -f "${DATA_DIR}/session.json" ]; then
    cat "${DATA_DIR}/session.json"
    echo ""
    echo "If session errors persist, delete with: rm ${DATA_DIR}/session.json"
else
    echo "OK: no session file (will start fresh)"
fi
```

## 6. Stale hooks in settings.json
Check if the global `~/.claude/settings.json` has PreToolUse hooks pointing to binaries that don't exist:
```bash
SETTINGS="${HOME}/.claude/settings.json"
if [ -f "$SETTINGS" ]; then
    echo "Settings file: $SETTINGS"
    cat "$SETTINGS"
    echo ""
    # Extract hook commands and check if binaries exist
    grep -oE "'[^']+'" "$SETTINGS" | tr -d "'" | while read -r bin; do
        if [ ! -x "$bin" ]; then
            echo "FAIL: hook references missing binary: $bin"
        else
            echo "OK: hook binary exists: $bin"
        fi
    done
else
    echo "OK: no settings file"
fi
```

## 7. Daemon status
```bash
CC_BIN="${HOME}/.crustyclaw/bin/crustyclaw"; [ -x "$CC_BIN" ] || CC_BIN="${CLAUDE_PLUGIN_ROOT}/bin/crustyclaw"; "$CC_BIN" statusline 2>/dev/null || echo "Daemon not running"
```

## 8. CLAUDECODE env leak
Check if the current environment would leak CLAUDECODE to a daemon started from here:
```bash
if [ -n "$CLAUDECODE" ]; then
    echo "WARN: CLAUDECODE is set — daemon must be started with 'env -u CLAUDECODE' to avoid nested session errors"
else
    echo "OK: CLAUDECODE not set"
fi
```

## 9. Lock file
```bash
DATA_DIR="${CRUSTYCLAW_DATA_DIR:-$HOME/.crustyclaw}"
if [ -f "${DATA_DIR}/daemon.lock" ]; then
    if pgrep -f "crustyclaw$" >/dev/null 2>&1; then
        echo "OK: lock file exists and daemon is running"
    else
        echo "WARN: stale lock file (daemon not running) — safe to ignore, will be replaced on next start"
    fi
else
    echo "OK: no lock file"
fi
```

After running all checks, summarize the results. For any FAIL items, suggest the fix. If the user confirms, apply the fixes (e.g. delete stale session, clean up settings.json, etc).
