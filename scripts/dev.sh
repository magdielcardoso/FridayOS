#!/usr/bin/env bash
# FridayOS dev loop: rebuild and relaunch on source changes.
# GPUI has no code hot-swap, so we do the next best thing: incremental rebuild
# plus a fast process restart (usually ~1-2 s once the first build is warm).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

APP_PID=""

kill_app() {
  if [[ -n "${APP_PID}" ]] && kill -0 "${APP_PID}" 2>/dev/null; then
    kill "${APP_PID}" 2>/dev/null
    wait "${APP_PID}" 2>/dev/null
  fi
  APP_PID=""
}

cleanup() {
  kill_app
  exit 0
}
trap cleanup INT TERM

echo "[friday] initial build…"
if ! cargo build -p friday-app -p friday-tools; then
  echo "[friday] initial build failed — waiting for changes"
else
  "$ROOT/target/debug/friday-app" &
  APP_PID=$!
  echo "[friday] running (pid ${APP_PID})"
fi

stamp() {
  { stat -f '%m' "$ROOT/Cargo.toml" 2>/dev/null
    find "$ROOT/crates" -type f \( -name '*.rs' -o -name '*.toml' \) -print0 \
      | xargs -0 stat -f '%m %N' 2>/dev/null
  } | sort | md5
}

LAST="$(stamp)"

while true; do
  sleep 0.7
  NOW="$(stamp)"
  [[ "$NOW" == "$LAST" ]] && continue
  LAST="$NOW"
  sleep 0.15
  echo "[friday] change detected — rebuilding"
  if cargo build -p friday-app -p friday-tools; then
    kill_app
    "$ROOT/target/debug/friday-app" &
    APP_PID=$!
    echo "[friday] relaunched (pid ${APP_PID})"
  else
    echo "[friday] build failed — keeping previous instance"
  fi
done
