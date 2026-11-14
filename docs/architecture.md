# FridayOS — architecture

## Overview

```
┌──────────────────────────── friday-app (GPUI) ────────────────────────────┐
│  window · title bar · transcript · input · status line · voice meters      │
└───────────────▲───────────────────────────────┬───────────────────────────┘
                │ events (msg, delta, done)     │ commands (send, cancel)
┌───────────────┴───────────────────────────────▼───────────────────────────┐
│                              friday-core                                  │
│  auth (Codex OAuth)   codex client   persona/prompts   voice pipeline      │
└───────────────▲───────────────────────────────┬───────────────────────────┘
                │                               │
        ~/.codex/auth.json              chatgpt.com / auth.openai.com
```

`friday-core` has no UI dependencies. `friday-app` owns GPUI and translates
core events into rendered state on the GPUI foreground thread.

## Auth (Codex subscription)

- Source of truth: `~/.codex/auth.json` (`auth_mode: "chatgpt"`,
  `tokens.access_token`, `tokens.refresh_token`, `tokens.account_id`).
- Tokens are JWT access tokens; refresh through the OpenAI OAuth token endpoint
  when expired (see protocol notes), then write the file back atomically with
  mode `0600`.
- All backend calls carry `Authorization: Bearer <access_token>` and
  `ChatGPT-Account-Id: <account_id>`.

## Model transport

Codex exposes its models through the ChatGPT backend, not the public API.
Two viable transports:

1. **Direct HTTP** to the Codex responses endpoint (streamed SSE).
2. **`codex app-server --stdio`** JSON-RPC, which is what the Codex CLI and
   MeuZe use. This gives reasoning items, tool calls and approvals for free.

Decision: start with the transport that yields the smallest working client and
keep the interface (`CodexClient::stream_turn`) transport-agnostic.

## Voice pipeline (phase 2)

Mirrors MeuZe's realtime call:

1. Capture mic at 16 kHz mono, 4096-sample frames (~256 ms).
2. RMS VAD: `speechThreshold ≈ 0.014`, `bargeThreshold ≈ 0.035`, end utterance
   after 2 silent frames, cap 117 frames (~20 s).
3. Encode WAV PCM16 → ChatGPT transcribe endpoint → text.
4. Send text turn to Codex, stream deltas.
5. Chunk streamed text into sentences, synthesize each (ChatGPT read-aloud TTS),
   play through output device; barge-in cancels playback and the turn.
6. Loop.

## Concurrency

- One GPUI foreground thread. Network/tasks run on GPUI's background executor
  (`cx.background_spawn`) and send state back through `Entity::update`.
- Turns are cancellable: a cancel token aborts the HTTP stream and clears the
  TTS queue.

## Rendering contract

- Pure black background, red phosphor text; ASCII ornament instead of icons.
- No animation library; a small tick timer drives the starfield/blink.
- Text is laid out on a character grid; monospace everywhere.
