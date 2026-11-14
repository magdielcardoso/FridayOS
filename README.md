```
  ______ _____  _____           _   _  ____   _____
 |  ____|  __ \|_   _|         | \ | |/ __ \ / ____|
 | |__  | |__) | | |  _ __ _   |  \| | |  | | (___
 |  __| |  _  /  | | | '_ \ \ / / . ` | |  | |\___ \
 | |    | | \ \ _| |_| | | \ V /| |\  | |__| |____) |
 |_|    |_|  \_\_____|_| |_|\_/ |_| \_|\____/|_____/
```

# FridayOS

A JARVIS-like desktop assistant for macOS, written in Rust and GPUI.

Friday is powered by the **Codex subscription** (ChatGPT plan) — the same
backend the Codex CLI uses. No OpenAI API key required: Friday reads the
credentials Codex already stores in `~/.codex/auth.json`.

## Status

Early scaffold. Phase 1 goal: talk to the Codex model and get a streamed
answer. Voice (mic → STT → model → TTS) comes next, then tools.

## Aesthetic

Terminal, not dashboard.

- Pure black substrate (`#000000`), red phosphor accent (`#FF2F2F`).
- ASCII art, box drawing, monospace grid. No rounded cards, no shadows.
- One theme only — there is no light mode and there never will be.

## Layout

```
crates/friday-core   auth, Codex client, prompts, (later) voice pipeline
crates/friday-app    GPUI desktop shell for macOS
docs/                architecture + protocol notes
scripts/dev.sh       rebuild-and-relaunch loop (hot reload-ish)
```

## Run

```sh
./scripts/dev.sh          # watch sources, rebuild, relaunch
cargo run -p friday-app   # plain run
```

Requires a ChatGPT login already present via the Codex CLI (`codex login`).

## Git

Branch `develop`, remote `origin` → github.com/magdielcardoso/FridayOS.
