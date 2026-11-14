# Codex + ChatGPT subscription protocol (as used by FridayOS)

Reverse-engineered from MeuZe (`~/Developer/MeuZe`) and verified against the
local Codex CLI. Two different transports are involved:

| Purpose | Transport |
|---|---|
| Chat / reasoning / future tools | `codex app-server --stdio` (JSON-RPC 2.0, one JSON per line) |
| Speech to text | `POST https://chatgpt.com/backend-api/transcribe` |
| Text to speech | `POST https://chatgpt.com/backend-api/read-aloud/synthesize` |

All of it bills against the ChatGPT plan stored in `~/.codex/auth.json`.
There is no API key path.

## 1. Credentials

`$CODEX_HOME/auth.json` or `~/.codex/auth.json`:

```json
{
  "auth_mode": "chatgpt",
  "OPENAI_API_KEY": null,
  "tokens": {
    "access_token": "<JWT>",
    "refresh_token": "<opaque>",
    "id_token": "<JWT>",
    "account_id": "acc-…"
  },
  "last_refresh": "2026-11-14T12:34:56Z"
}
```

- Expiry: `exp` claim of `access_token`.
- Account id: `tokens.account_id`, fallback to id-token claim
  `https://api.openai.com/auth.chatgpt_account_id`.
- Plan: claim `https://api.openai.com/auth.chatgpt_plan_type`.
- Email: claim `https://api.openai.com/profile.email`.

Refresh (when `exp` is within 60 s, or after a 401):

```http
POST https://auth.openai.com/oauth/token
Content-Type: application/x-www-form-urlencoded

grant_type=refresh_token
client_id=app_EMoamEEZ73f0CkXaXp7hrann
refresh_token=<opaque>
```

Rotate `refresh_token` if returned, write the file back atomically (0600).
Friday does this in `friday-core/src/auth.rs` and shares the file with the CLI.

Headers for every direct backend call:

```http
Authorization: Bearer <access_token>
originator: Codex Desktop
User-Agent: Codex Desktop/0.1.0 (Mac OS; arm64)
ChatGPT-Account-Id: <account_id>
```

## 2. Chat via `codex app-server --stdio`

```
initialize  → {"clientInfo":{"name":"friday","title":"FridayOS","version":"0.1.0"},
               "capabilities":{"experimentalApi":false,"requestAttestation":false}}
initialized → notification
thread/start → {cwd, approvalPolicy:"on-request", sandbox:"workspace-write",
                ephemeral:false, model?, developerInstructions?}
turn/start   → {threadId, input:[{type:"text",text,text_elements:[]}], cwd,
                approvalPolicy, sandboxPolicy:{type:"workspaceWrite",…}, effort?}
```

Notifications we consume:

```json
{"method":"item/agentMessage/delta","params":{"delta":"…","itemId":"…"}}
{"method":"item/reasoning/summaryTextDelta","params":{"delta":"…"}}
{"method":"item/completed","params":{"item":{"type":"agentMessage","text":"…"}}}
{"method":"turn/completed","params":{"turn":{"id":"…","status":"completed"}}}
{"method":"error","params":{"error":{"message":"…"}}}
```

Server-initiated approval requests carry an `id` and a `method` such as
`item/commandExecution/requestApproval`; reply
`{"id":…,"result":{"decision":"decline"}}` while tools are disabled.

Cancellation is process-level: kill the CLI. Friday relaunches and starts a
fresh thread (the UI keeps the transcript).

## 3. Speech to text

```http
POST https://chatgpt.com/backend-api/transcribe
Accept: application/json
multipart/form-data:
  file=friday-ditado.wav (audio/wav, PCM16 mono 16 kHz)
  language=pt
  dictation_session_id=<uuid4>
  attempt_id=<uuid4>
  duration_ms=<int>
```

Response `{"text":"…"}`. One retry after a forced token refresh on 401.
Errors: 429 rate limit, 403 no entitlement, `audio_too_short`.

## 4. Text to speech ("read aloud")

```http
POST https://chatgpt.com/backend-api/read-aloud/synthesize
Accept: audio/*
Content-Type: application/json

{"text":"…"}
```

Returns encoded audio (typically AAC). Decode to PCM16 with
`/usr/bin/afconvert -f WAVE -d LEI16 -c 1`, then play at the WAV's own sample
rate. No voice id or speed parameters exist — the account's default voice is
the only one.

## 5. Voice turn-taking (target behaviour, from MeuZe)

- Capture 16 kHz mono in 4096-sample frames (256 ms).
- RMS VAD: start after 2 frames ≥ 0.014; end after 2 silent frames; cap 117 frames.
- Barge-in: 4 consecutive frames ≥ 0.035 while Friday speaks or thinks.
- Stream deltas into a sentence chunker (220 chars max, sentence boundaries
  `[.!?…]`, partial flush every 450 ms) and synthesize each sentence in order.
- Turn closes after 1.8 s with no delta (25 s hard timeout).
- Interrupt = stop playback and kill the Codex turn.
