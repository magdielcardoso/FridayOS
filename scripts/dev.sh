#!/usr/bin/env bash
# FridayOS dev loop: rebuild the .app bundle and relaunch on source changes.
# GPUI has no code hot-swap, so we do the next best thing: incremental rebuild
# plus a fast relaunch (usually a couple of seconds once the first build is warm).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

APP_PID=""

kill_app() {
  pkill -f "FridayOS.app/Contents/MacOS/FridayOS" 2>/dev/null
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

launch() {
  kill_app
  open -n "$ROOT/dist/FridayOS.app"
  sleep 1
  APP_PID="$(pgrep -f 'FridayOS.app/Contents/MacOS/FridayOS' | head -1)"
  echo "[friday] running (pid ${APP_PID:-?})"
}

echo "[friday] initial build…"
if ./scripts/bundle.sh >/dev/null; then
  launch
else
  echo "[friday] initial build failed — waiting for changes"
fi

stamp() {
  { stat -f '%m' "$ROOT/Cargo.toml" 2>/dev/null
    find "$ROOT/crates" -type f \( -name '*.rs' -o -name '*.toml' -o -name '*.swift' \) -print0 \
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
  echo "[friday] mudança detectada — recompilando"
  if ./scripts/bundle.sh >/dev/null; then
    launch
  else
    echo "[friday] build falhou — mantendo a instância anterior"
  fi
done
