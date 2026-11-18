# 0019. Telegram voice: speech models served by mlx-air, called through llm/

- Status: Accepted
- Date: 2026-11-18

## Context

The owner uses gm from the phone through Telegram ([0006](0006-telegram-as-mobile-channel.md)).
Typing is the bottleneck away from a keyboard, so Phase 5 adds voice: the owner sends voice notes and
can hear answers. The Telegram Bot API has no live calls for bots. Voice reaches a bot as a finished
voice note (OGG/Opus, `message.voice`), and a bot answers with `sendVoice`, so every exchange is one
clip in and one clip out.

The text flow already handles routing, approvals, audit, lanes and model residency
([0003](0003-intent-router-before-agent.md), [0010](0010-llm-client-lanes-residency.md)). A full-duplex
speech-to-speech model would replace that flow with audio that no part of gm can inspect, approve or
audit. It also has no use for full duplex on a channel that only delivers whole clips.

mlx-air runs one model per instance and owns model processes; gm decides when they run and calls
their endpoints ([`model.rs`](../../crates/gm/src/model.rs)). mlx-air serves only
`POST /v1/chat/completions` today.

## Decision

- **Voice wraps the text flow.** A voice note is transcribed (speech-to-text), and the transcript
  takes the same path as typed text: commands, replies to bot messages, `user.instruction`. A voice
  answer is the text answer read aloud (text-to-speech). The text message is always sent and stays
  the source of truth: buttons, approvals and the exact text of a reply draft live only there.
- **mlx-air serves the speech models.** It gains OpenAI-style `POST /v1/audio/transcriptions` and
  `POST /v1/audio/speech`. Each speech model runs as its own mlx-air instance. The endpoints accept
  and return OGG/Opus, so gm never links an audio codec. The contract gm relies on is in
  [speech-runtime.md](../spec/speech-runtime.md).
- **llm/ is the only model path.** Speech calls are `transcribe` and `speak` in
  [`llm/`](../../crates/gm/src/llm/mod.rs), on the `interactive` lane. They share the lane scheduler,
  in-flight counting, the start-on-demand hook of the model control, and the unavailable handling
  with chat calls.
- **Residency follows the LLM.** The model control manages the speech instances with the same model
  hours, start on owner request and idle stop as the chat model. A voice note outside model hours
  starts the speech-to-text instance and waits for it like any owner task.
- **Models:** speech-to-text is Whisper large-v3-turbo (one model for en / vi / ja, with language
  detection and English terms inside Vietnamese). Text-to-speech is Supertonic 3, the only single
  model found covering en / vi / ja. It is wired in only after a listening check on fixed en / vi / ja
  samples ([phase-5.md](../plan/phase-5.md#5a-speech-runtime)).
- **Audio is not stored.** Incoming and generated audio live in memory for one request. The
  transcript is kept like typed text. The only audio reference gm stores is the Telegram `file_id` of
  a clip it sent, so the clip can be resent without generating it again.

## Consequences

- Voice can never act without the safeguards of the text flow: a transcript is shown to the owner
  before its answer, and every external effect still needs Approve on an exact text.
- A speech-model failure degrades to text. The owner always gets the text answer.
- Two more model instances, about 2 GB together, run during model hours next to the chat model.
- gm cannot ship voice before mlx-air has the audio endpoints. The mlx-air work comes first.
- The model control manages several instances instead of one.

## Alternatives considered

- **A full-duplex speech-to-speech model** (for example Haru-4B): natural turn-taking, but no use
  for it on voice notes. It's a base model that doesn't follow gm's prompts or JSON output, and it
  produces no text for approvals or audit.
- **A separate mlx-audio server under its own launchd agent:** fastest to try, but a second model
  runtime with its own process management, next to mlx-air.
- **Speech models inside the gm binary** (whisper.cpp and an ONNX text-to-speech model): no extra
  service, but model memory and crashes would move into the process that runs the mail pipeline.
- **gm converts audio (libopus) or shells out to ffmpeg:** keeps mlx-air WAV-only, but adds a C
  dependency or an external binary to gm.
- **A separate SpeechClient:** a cleaner type boundary, but it would duplicate lanes, in-flight
  counting and outage handling.
- **PhoWhisper for speech-to-text:** best published Vietnamese accuracy, but weaker on Japanese and
  on English terms inside Vietnamese, and based on the slower large-v2.
- **Kokoro plus a Vietnamese text-to-speech model:** better-known en / ja voices, but two models and
  two different-sounding voices.
