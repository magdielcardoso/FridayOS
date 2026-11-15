#!/usr/bin/env bash
# permissions.sh - checklist de permissoes TCC da FridayOS.
#
# Este script NAO altera nada: apenas testa o que o processo atual consegue
# fazer e imprime OK / FALTA / -- (nao testado).
#
# Uso:
#   ./scripts/permissions.sh
#
# Observacao: os testes com osascript podem abrir o dialogo de permissao do
# macOS na primeira execucao. Responda Permitir para ver OK na proxima rodada.

set -uo pipefail

HOME_DIR="${HOME:-}"
OK_COUNT=0
FAIL_COUNT=0
SKIP_COUNT=0

report() {
  # $1 = OK | FALTA | --, $2 = nome, $3 = detalhe
  printf '  [%-5s] %-22s %s\n' "$1" "$2" "$3"
}

ok() {
  OK_COUNT=$((OK_COUNT + 1))
  report "OK" "$1" "$2"
}

fail() {
  FAIL_COUNT=$((FAIL_COUNT + 1))
  report "FALTA" "$1" "$2"
}

skip() {
  SKIP_COUNT=$((SKIP_COUNT + 1))
  report "--" "$1" "$2"
}

printf '\nFridayOS / permissoes do macOS\n'
printf 'processo: %s\n' "$0"
printf 'usuario:  %s\n' "${USER:-?}"
printf 'host:     %s\n\n' "$(hostname 2>/dev/null || echo '?')"

if [[ "$(uname -s 2>/dev/null)" != "Darwin" ]]; then
  printf 'Este script so faz sentido no macOS.\n'
  exit 0
fi

# ---------------------------------------------------------------------------
# 1. Acessibilidade (AX + eventos sinteticos)
# ---------------------------------------------------------------------------
AX_OUT="$(osascript -e 'tell application "System Events" to get name of first process' 2>/dev/null)"
if [[ -n "${AX_OUT}" ]]; then
  ok "acessibilidade" "System Events respondeu: ${AX_OUT}"
else
  fail "acessibilidade" "Ajustes > Privacidade e Seguranca > Acessibilidade"
fi

# ---------------------------------------------------------------------------
# 2. Gravacao de Tela
# ---------------------------------------------------------------------------
SHOT="$(mktemp -t friday-tcc 2>/dev/null || true)"
if [[ -n "${SHOT}" ]]; then
  SHOT="${SHOT}.png"
  if screencapture -x "${SHOT}" >/dev/null 2>&1 && [[ -s "${SHOT}" ]]; then
    BYTES="$(wc -c <"${SHOT}" 2>/dev/null | tr -d ' ')"
    ok "gravacao de tela" "screencapture gerou ${BYTES:-?} bytes"
  else
    fail "gravacao de tela" "Ajustes > Privacidade e Seguranca > Gravacao de Tela"
  fi
  rm -f "${SHOT}" 2>/dev/null
else
  skip "gravacao de tela" "mktemp falhou"
fi

# ---------------------------------------------------------------------------
# 3. Microfone (hardware/driver; captura real e testada no pipeline de voz)
# ---------------------------------------------------------------------------
AUDIO="$(system_profiler SPAudioDataType 2>/dev/null || true)"
if [[ -z "${AUDIO}" ]]; then
  skip "microfone" "system_profiler sem resposta"
elif printf '%s\n' "${AUDIO}" | grep -Eiq 'input|entrada'; then
  ok "microfone" "dispositivo de entrada listado pelo system_profiler"
else
  fail "microfone" "nenhuma entrada de audio encontrada"
fi

# ---------------------------------------------------------------------------
# 4. Reconhecimento de Fala (helper futuro; pula se ausente)
# ---------------------------------------------------------------------------
SPEECH_HELPER=""
for CANDIDATE in \
  "${HOME_DIR}/Library/Caches/FridayOS/bin/speech_auth" \
  "${HOME_DIR}/Library/Caches/FridayOS/bin/speech-check" \
  "${HOME_DIR}/Library/Caches/FridayOS/bin/speech_check"; do
  if [[ -x "${CANDIDATE}" ]]; then
    SPEECH_HELPER="${CANDIDATE}"
    break
  fi
done

if [[ -z "${SPEECH_HELPER}" ]]; then
  skip "reconhecimento de fala" "helper ausente (compilado sob demanda)"
elif "${SPEECH_HELPER}" --check >/dev/null 2>&1; then
  ok "reconhecimento de fala" "helper ${SPEECH_HELPER##*/} autorizado"
else
  skip "reconhecimento de fala" "helper presente, interface --check indisponivel"
fi

# ---------------------------------------------------------------------------
# 5. Acesso Total ao Disco
# ---------------------------------------------------------------------------
FDA_TESTED=0
for PROTECTED in \
  "${HOME_DIR}/Library/Mail" \
  "${HOME_DIR}/Library/Safari" \
  "${HOME_DIR}/Library/Messages"; do
  if [[ -d "${PROTECTED}" ]]; then
    FDA_TESTED=1
    if ls "${PROTECTED}" >/dev/null 2>&1; then
      ok "acesso total ao disco" "leitura permitida em ${PROTECTED}"
    else
      fail "acesso total ao disco" "Ajustes > Privacidade e Seguranca > Acesso Total ao Disco"
    fi
    break
  fi
done
if [[ "${FDA_TESTED}" -eq 0 ]]; then
  skip "acesso total ao disco" "nenhuma pasta protegida encontrada para testar"
fi

# ---------------------------------------------------------------------------
# 6. Automacao (Finder)
# ---------------------------------------------------------------------------
FINDER_OUT="$(osascript -e 'tell application "Finder" to get name' 2>/dev/null)"
if [[ -n "${FINDER_OUT}" ]]; then
  ok "automacao (Finder)" "respondeu: ${FINDER_OUT}"
else
  fail "automacao (Finder)" "Ajustes > Privacidade e Seguranca > Automacao"
fi

# ---------------------------------------------------------------------------
# Resumo
# ---------------------------------------------------------------------------
printf '\nresumo: %d ok, %d falta, %d nao testado\n' \
  "${OK_COUNT}" "${FAIL_COUNT}" "${SKIP_COUNT}"
printf 'detalhes: docs/permissions.md\n'
printf 'nada foi alterado por este script.\n\n'

exit 0
