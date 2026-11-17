#!/usr/bin/env bash
# FridayOS dev loop: rebuild the .app bundle and relaunch on source changes.
# GPUI has no code hot-swap, so we do the next best thing: incremental rebuild
# plus a fast relaunch (usually a couple of seconds once the first build is warm).
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT" || exit 1

# Um dev loop por repositório. Sem isso, duas cópias do script competiam:
# cada uma matava e reabria o app por conta própria — exatamente o efeito de
# "a Friday abre sozinha toda hora".
LOCK="$ROOT/.dev-loop.lock"
if ! mkdir "$LOCK" 2>/dev/null; then
  echo "[friday] já existe um dev loop rodando neste repositório — saindo"
  exit 1
fi

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
  rmdir "$LOCK" 2>/dev/null
  exit 0
}
trap cleanup INT TERM
trap 'rmdir "$LOCK" 2>/dev/null' EXIT

launch() {
  # Sem `-n`: `open -n` abre sempre uma cópia nova. Era isso que deixava
  # várias Fridays vivas ao mesmo tempo, cada uma reabrindo a própria janela.
  kill_app
  open "$ROOT/dist/FridayOS.app"
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
