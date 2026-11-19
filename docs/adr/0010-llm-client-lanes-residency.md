# 0010. Direct LLM client with priority lanes and model hours; no gateway

- Status: Accepted
- Date: 2026-11-16
- Supersedes: [0004](0004-openai-compatible-llm-via-litellm.md)

## Context

The original plan put LiteLLM between gm and mlx-server-runtime and ran two models. Since then the
owner chose one main model (see [0011](0011-model-qwen3-8b-now-bonsai-later.md)), local-only inference,
and a model that is resident only 09:00–19:00, started earlier only for the owner's own requests. The measured workload is ~20 mails/day, plus occasional
interactive requests. mlx-server-runtime offers continuous batching, chunked prefill and a prefix KV
cache, but no `tools`, `response_format` or JSON mode, and returns 429 under backpressure.

## Decision

- **No gateway.** All calls go through gm's `LlmClient` (OpenAI-compatible `POST /v1/chat/completions`
  to `GM_LLM_BASE_URL`). It records tokens, latency and outcome per task and lane in the
  `llm_calls` table and in metrics.
- **Lanes:** `interactive` (user requests, reply drafts) > `notify` (triage and summary of incoming
  mail) > `batch` (digest items). A small in-process scheduler admits at most
  `GM_LLM_MAX_INFLIGHT` requests (default 2), always from the highest non-empty lane. Batch work
  yields to the other lanes.
- **Model hours:** gm runs model work only between `GM_MODEL_ON` (09:00) and `GM_MODEL_OFF`
  (19:00), local time. Jobs that need the model outside them are deferred (`run_after` = next
  on-time), not failed ([`clock.rs`](../../crates/gm/src/clock.rs) `ModelHours`,
  [`pipeline.rs`](../../crates/gm/src/pipeline.rs) `Pipeline::model_step`). Model hours govern
  background work only: a task the owner starts from Telegram (translate, analyze, reply, on the
  `interactive` lane) runs at any hour, whenever the model answers, because the owner is waiting for
  it and asked for it; if the model is not answering, the owner is told once and the task waits like
  any other job ([`tasks/runtime.rs`](../../crates/gm/src/tasks/runtime.rs)).
- **Model process:** run by mlx-air (instance `gm`, its own launchd agent); gm decides *when* it
  runs and only calls the `mlx-air` CLI: `mlx-air serve --model <id> --port <port> --detach --name
  gm` to start and `mlx-air stop gm` to stop (`stop` also removes the instance's launchd agent, so
  KeepAlive does not bring it back). The model id is `GM_LLM_MODEL` and the port is the one in
  `GM_LLM_BASE_URL`; gm manages only a model on this machine. Code:
  [`model.rs`](../../crates/gm/src/model.rs).
  - **Inside model hours** the model is up: a loop (every 30 s) starts it when it is down.
  - **Outside model hours** the loop stops it, after in-flight requests finish (at most the stop
    grace, default 5 minutes). An owner request (the `interactive` lane: a Telegram task or a web
    request) starts it first (a cold start of 20–30 s; the owner is told once), and it stays up until
    it has been idle for the idle minutes (default 10). Background lanes never start the model, and
    do not keep it up. Mail does not start it either: urgent mail waits for model hours (see
    [0015](0015-notification-policy-and-digest.md)).
  - **Manual rest:** `/rest` stops the local mlx-air instance immediately and blocks scheduled and
    on-demand starts. `/wakeup` starts it at any hour and restores the usual policy once `/ready`
    answers. The Start model and Stop model buttons on the Model tab use the same hold in
    `system_settings.model_resting`. Manual power controls apply to the local instance even when
    automatic management is off.
  - **Settings** are machine-wide, edited on the Model tab and stored in
    `system_settings.model_control`; `GM_MODEL_*` in `.env` are the first defaults: model hours,
    the automatic management switch (on by default; switching it off starts a stopped model unless
    it is resting), idle minutes, stop grace and the model id. A changed model id applies at the next
    start; until then requests carry the id the running server reports on `/ready`.
  - **If the model cannot be started** (mlx-air fails, or `/ready` does not answer within
    `GM_MODEL_START_TIMEOUT_S`), the task waits like any other job while the model is unavailable.
- **Prompt shape:** a shared, stable system prefix (role, untrusted-content rules, output rules),
  then the task block, then user settings, then the mail last inside `<email>` delimiters that the
  content cannot close. Prompts are versioned files under `prompts/`. This maximizes prefix-cache
  reuse across tasks.
- **Output:** one JSON object requested in the prompt; tolerant parse (strip `<think>`, code fences,
  take the outermost object, validate). On failure, retry once with a format reminder, then keep the
  raw text as the summary (`parse_ok: false`). An unavailable model (not running, timeout, 429/502/503/504) queues the job: it retries every
  `GM_LLM_UNAVAILABLE_RETRY_S` (5 min) without using an attempt and waits indefinitely; other LLM
  errors retry with backoff and then leave the job `dead`.
- **Thinking** follows the selected model client's policy by task tier ([ADR-0018](0018-llm-inference-profile.md)).
- **Input:** the mail is cleaned (HTML to text, quoted replies, signatures and footers removed), then
  truncated to `GM_LLM_MAX_INPUT_TOKENS` (≈2,000). Outputs are capped per task (`max_tokens`).

- **Later, in mlx-server-runtime (not in gm):** constrained JSON decoding and native tool calling.
  Until then gm relies on neither.

## Consequences

- One fewer service; per-task budgets and metrics live in gm itself.
- Mail arriving at night is summarized in the morning; only rule-only notifications go out without
  the model (see [0015](0015-notification-policy-and-digest.md)).
- Changing the model or endpoint stays a configuration change.

## Alternatives considered

- **LiteLLM proxy:** budgets, keys and fallbacks for many models and teams; unnecessary for one model
  and one user.
- **gm driving `launchctl` itself:** duplicated mlx-air's process management; gm now calls the
  `mlx-air` CLI instead and mlx-air still owns the process, its limits and its agent.
- **A schedule inside mlx-air or two launchd calendar agents:** no change in gm, but nothing could
  start the model for an owner request at night.
- **Always-resident model:** simpler, but holds ~5 GB of RAM (Qwen3-8B 4-bit) around the clock.
- **Load on demand for everything:** frees the most RAM, but adds a 20–30 s cold start to every
  first request, including mail summaries; only owner requests outside model hours start it.
