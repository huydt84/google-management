# 0010. Direct LLM client with priority lanes and model hours; no gateway

- Status: Accepted
- Date: 2026-11-16
- Supersedes: [0004](0004-openai-compatible-llm-via-litellm.md)

## Context

The original plan put LiteLLM between gm and mlx-server-runtime and ran two models. Since then the
owner chose one main model (see [0011](0011-model-qwen3-8b-now-bonsai-later.md)), local-only inference,
and a model that is resident only 09:00–19:00. The measured workload is ~20 mails/day, plus occasional
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
  [`pipeline.rs`](../../crates/gm/src/pipeline.rs) `Pipeline::model_step`).
- **Model process:** owned by mlx-air (instance `gm`, its own launchd agent with KeepAlive). gm
  never starts or stops it; it only probes `GM_LLM_READY_URL` for the health check.
  *Amended 2026-11-17:* the Phase 2 residency controller that started and stopped
  the model with `launchctl` duplicated mlx-air and was removed. Whether the model is unloaded at
  night is now an mlx-air setting.
- **Prompt shape:** a shared, stable system prefix (role, untrusted-content rules, output rules),
  then the task block, then user settings, then the mail last inside `<email>` delimiters that the
  content cannot close. Prompts are versioned files under `prompts/`. This maximizes prefix-cache
  reuse across tasks.
- **Output:** one JSON object requested in the prompt; tolerant parse (strip `<think>`, code fences,
  take the outermost object, validate). On failure, retry once with a format reminder, then keep the
  raw text as the summary (`parse_ok: false`). An unavailable model (not running, timeout, 429/502/503/504) queues the job: it retries every
  `GM_LLM_UNAVAILABLE_RETRY_S` (5 min) without using an attempt and waits indefinitely; other LLM
  errors retry with backoff and then leave the job `dead`.
- **Thinking** is disabled (`/no_think`) except for reply drafting and analysis.
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
- **gm starting and stopping the model (built in Phase 2, removed):** duplicated mlx-air's
  process management.
- **Always-resident model:** simpler, but holds ~8 GB of RAM around the clock (the current mlx-air
  setup).
- **Load on demand:** frees the most RAM, but adds a 20–30 s cold start to every first request.
