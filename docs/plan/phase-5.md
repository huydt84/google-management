# Phase 5: Telegram voice

**Goal:** the owner can talk to the bot and hear its answers. A voice note is transcribed and handled
like typed text. Answers to the owner's requests can be read aloud.

Prerequisite: Phase 3 (Telegram commands, tasks, approvals).

Architecture: [ADR-0019](../adr/0019-voice-through-mlx-air.md). Runtime contract:
[speech-runtime.md](../spec/speech-runtime.md).

Order: 5A, then 5B, then 5C. Each step is usable on its own: after 5B the owner can ask by voice and
read the answer; 5C adds the spoken answer.

## Behavior

### Voice in

- The poller accepts `message.voice` from the owner's chat, with the same chat check as text.
- Voice notes longer than **60 seconds** (`voice.duration`, checked before download) get a short
  localized reply asking to split the note, and are not transcribed.
- gm downloads the note with `getFile` (the file URL contains the bot token and is never logged),
  transcribes it through `llm/` on the `interactive` lane, and replies first with
  `🎙 <transcript>` (localized label) so a mishearing is visible.
- Then the transcript goes the same way typed text goes: a command, a reply to a bot message
  (`user.instruction`), or the help text. Every external effect still waits for Approve on the exact
  text.
- The transcript is kept like typed text in the same event payloads. The audio is not stored.

### Voice out

- **When:** an answer to a voice note is sent as text, then as a voice clip. An answer to typed text
  is text only. Every answer to an owner request carries a **🔊** button that sends a clip of that
  message on demand.
- **Which messages:** answers to owner requests only: command answers, and Translate, Analyze and
  Reply results, including reply-draft previews. The Approve button stays on the text message. Mail
  notifications, the digest, calendar notices and health alerts have no voice and no 🔊.
- **What is spoken:** the text message after a deterministic cleanup (emoji and markup dropped,
  list items read as separate sentences), capped at about **90 seconds** of speech. Longer text ends
  with a localized "the rest is in the message". No extra model call.
- **Voice language:** the language of the spoken text: the task's target language when known
  (Translate), otherwise detected ([`llm/language.rs`](../../crates/gm/src/llm/language.rs)). One
  configured voice per language (en, vi, ja).
- **Reuse:** gm stores the Telegram `file_id` of the clip it sent on the message's `chat_messages`
  row. A second 🔊 tap resends that `file_id` without generating audio again.

### Residency and outages

- The model control manages the speech instances with the chat model's rules: up during model hours,
  started by an owner request outside them, stopped after the idle minutes
  ([ADR-0010](../adr/0010-llm-client-lanes-residency.md)).
- A voice note that finds speech-to-text stopped tells the owner once that voice is starting, starts
  the instance and waits for it. If it is not ready within **60 seconds**, or the call fails, gm
  replies that it could not transcribe and asks for text. The note is not queued.
- If text-to-speech fails, the text answer is sent anyway, with a short "voice unavailable" note.

## 5A Speech runtime

Repository: mlx-air (`mlx-server-runtime`), not this one.

1. Listening check before wiring anything: Supertonic 3 on fixed en / vi / ja sample texts (a
   Translate result, an Analyze result with amounts and dates, a reply draft), and Whisper
   large-v3-turbo on short en / vi / ja voice notes, including Vietnamese with English terms. If
   Supertonic 3's Vietnamese or Japanese is not acceptable, the owner picks a replacement before 5C.
2. Implement [speech-runtime.md](../spec/speech-runtime.md) and its acceptance checks.

## 5B Voice in

Files: [`telegram/`](../../crates/gm/src/telegram/) (voice messages, `getFile`),
[`llm/`](../../crates/gm/src/llm/mod.rs) (`transcribe`), [`model.rs`](../../crates/gm/src/model.rs)
(more than one managed instance), `config.rs`, locales.

- Speech-to-text model id and endpoint are configured like the chat model's.
- Tests: wiremock for Telegram `getFile` and file download and for `/v1/audio/transcriptions`.
  Cover the 60 s cap, transcript echo, routing of a transcript as a command and as a reply,
  the cold start, and the text fallback.

## 5C Voice out

Files: [`telegram/`](../../crates/gm/src/telegram/) (`sendVoice`, 🔊 callback),
[`llm/`](../../crates/gm/src/llm/mod.rs) (`speak`), the task reply path, a migration adding the clip
`file_id` to `chat_messages`, locales.

- Text-to-speech model id, endpoint and one voice per language are configured like the chat model's.
- Tests: wiremock for `/v1/audio/speech` and `sendVoice`. Cover voice in → voice out, typed in →
  text only, the 🔊 button with `file_id` reuse, the cleanup and length cap, the voice language, no
  🔊 on notifications, and the text fallback.

## Docs updated with the code

Each workstream updates, in the same commit: [interfaces.md](../spec/interfaces.md) (Telegram
voice, settings), [data-model.md](../spec/data-model.md) (`chat_messages`),
[events.md](../spec/events.md) if a topic changes, [security.md](../spec/security.md) (audio is
untrusted input, never stored or logged), and the module map and phase table in
[architecture.md](../architecture.md).

## Checks

Automated tests never call real Telegram or model endpoints. Live check after each workstream, with
`gm serve` running and the speech instances up: send en / vi / ja voice notes from the owner's
Telegram, and check the transcript echo, the answer, the clip and its language. Then tap 🔊 twice
and check that the second tap makes no `/v1/audio/speech` call in the logs. Then stop the speech
instance and check the text fallback.
