#!/usr/bin/env bash
# Dev-only WKWebView self-test. Usage: scripts/selftest.sh <log> <datadir> <file>...
# Uses a scratch library (THOTH_DATA_DIR); never touches the real app data.
set -u
LOG="$1"; DATA="$2"; shift 2
FILES="$(IFS=:; echo "$*")"
pkill -f "target/debug/thoth-reader" 2>/dev/null
pkill -f "tauri dev" 2>/dev/null
for p in $(lsof -tiTCP:1420 -sTCP:LISTEN 2>/dev/null); do kill "$p"; done
sleep 1
rm -rf "$DATA"
THOTH_DATA_DIR="$DATA" THOTH_SELFTEST="$FILES" npx tauri dev > "$LOG" 2>&1 &
for _ in $(seq 1 400); do
  grep -qE 'selftest done|error\[|panicked|terminated with a non-zero' "$LOG" && break
  sleep 3
done
grep -E '^\[webview|panicked|error\[|non-zero' "$LOG" | cut -c1-1500
