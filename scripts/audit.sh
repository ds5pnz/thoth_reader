#!/usr/bin/env bash
# Layout audit of the PDF translated page on random pages (mock Korean filler, numbers only).
# usage: scripts/audit.sh <books-dir> <log> <scratch-datadir> [only-doc-indices e.g. 8,10]
set -u
B="$1"; LOG="$2"; DATA="$3"; ONLY="${4:-}"
pkill -f mock_openai_server.py 2>/dev/null
python3 -I "$(dirname "$0")/mock_openai_server.py" 18080 0 filler > /dev/null 2>&1 &
sleep 1
F=(); for n in 1 3 5 13 18 24 30 33; do F+=("$(ls "$B"/*.pdf | sort | sed -n ${n}p)"); done
export THOTH_SELFTEST_SCENARIO=audit THOTH_SELFTEST_PAGE=10 THOTH_SELFTEST_PROVIDER=http://127.0.0.1:18080/v1 THOTH_SELFTEST_WAIT_MS=60000 THOTH_SELFTEST_TICKS=1100
[ -n "$ONLY" ] && export THOTH_SELFTEST_ONLY="$ONLY"
"$(dirname "$0")/selftest.sh" "$LOG" "$DATA" "${F[@]}" > /dev/null 2>&1
pkill -f mock_openai_server.py 2>/dev/null
pkill -f "target/debug/thoth-reader" 2>/dev/null; pkill -f "tauri dev" 2>/dev/null
for p in $(lsof -tiTCP:1420 -sTCP:LISTEN 2>/dev/null); do kill "$p"; done
