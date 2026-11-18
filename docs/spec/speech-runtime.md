# Speech runtime contract (mlx-air)

What gm needs from mlx-air to support Telegram voice ([ADR-0019](../adr/0019-voice-through-mlx-air.md),
[Phase 5](../plan/phase-5.md)). This work belongs in mlx-air (`mlx-server-runtime`). gm calls these
endpoints only through [`llm/`](../../crates/gm/src/llm/mod.rs) and starts and stops the instances
through the `mlx-air` CLI ([`model.rs`](../../crates/gm/src/model.rs)), as it already does for the
chat model.

Status: planned. Nothing here is implemented in mlx-air or gm yet.

## Instances

- One model per instance, as for chat models: `mlx-air serve --model <id> --port <port> --detach
  --name <name>`, stopped with `mlx-air stop <name>`.
- `mlx-air serve` detects the model type (chat, speech-to-text, text-to-speech) and exposes only the
  matching endpoint. A request to an endpoint the instance does not serve returns `404`.
- `GET /ready` behaves as for chat instances: `200` once the model is loaded, and the response names
  the model id.
- Listens on `127.0.0.1` only.

## `POST /v1/audio/transcriptions`

Speech-to-text, shaped like the OpenAI audio API.

Request: `multipart/form-data`.

| Field | Required | Notes |
|-------|----------|-------|
| `file` | yes | Audio bytes. Must accept OGG/Opus (Telegram voice notes, mono, 48 kHz). WAV support is optional. |
| `model` | yes | Model id; must match the served model. |
| `language` | no | ISO 639-1 hint (`en`, `vi`, `ja`). Without it, the model detects the language. |
| `response_format` | no | `json` (default) or `verbose_json`. |

Response `200` (`json`):

```json
{"text": "…", "language": "vi", "duration": 12.4}
```

`language` (detected or hinted) and `duration` (seconds of input audio) are required in the
response, including with `json`. gm uses them for the reply language and its length check.

## `POST /v1/audio/speech`

Text-to-speech, shaped like the OpenAI audio API.

Request: `application/json`.

| Field | Required | Notes |
|-------|----------|-------|
| `model` | yes | Model id; must match the served model. |
| `input` | yes | Plain text, at most 4,096 characters. |
| `voice` | yes | A voice name the model provides. gm passes one voice per language from its configuration. |
| `language` | no | ISO 639-1 (`en`, `vi`, `ja`) for models that need it. |
| `response_format` | no | `opus` (gm always sends it): OGG container with Opus audio, mono, which Telegram's `sendVoice` plays inline. `wav` is optional. |

Response `200`: the audio bytes, with `Content-Type: audio/ogg` for `opus`.

## Errors and limits

Same conventions as `/v1/chat/completions`, so gm's existing outage handling applies:

| Status | Meaning | gm's reaction |
|--------|---------|---------------|
| `400` | Undecodable audio, empty or too-long input, unknown voice | No retry; the owner gets a text notice. |
| `404` | Endpoint not served by this instance | Configuration error; logged. |
| `413` | Upload larger than the instance limit (at least 2 MB, enough for 60 s of voice) | No retry; text notice. |
| `429` | Queue full | Treated as unavailable. |
| `502`, `503`, `504` | Model not ready or failed | Treated as unavailable. |

Error bodies follow `{"error": {"message": "…", "type": "…"}}` and never echo the input text or
audio.

## Privacy

- No audio, input text or transcript is written to disk or logs. Logs carry only sizes, durations,
  the language and timings.
- No audio cache across requests.

## Acceptance checks for mlx-air

- Transcribe a Telegram-format OGG/Opus clip in each of en / vi / ja and get the right `language`.
- Synthesize a sentence in each of en / vi / ja as `opus`, and play the bytes inline as a Telegram
  voice note.
- `/ready` answers for both instance types. A chat request to a speech instance returns `404`.
