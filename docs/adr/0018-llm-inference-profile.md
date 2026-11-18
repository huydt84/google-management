# 0018. LLM inference profile: task tiers, parameters per model in llm.yaml

- Status: Accepted
- Date: 2026-11-18

## Context

Model tasks differ in how much reasoning they need: routing an instruction or triaging a mail is a
short classification, while analyzing a mail for risks or writing a reply benefits from thinking.
How a model is told to think differs per model (Qwen3 has a `/no_think` tag; other models have no
switch), and the model can be changed in Settings. The local runtime (mlx-server-runtime) accepts
only `temperature`, `top_p` and `max_tokens`: there is no reasoning-effort parameter, so the only
levers are leaving the no-think tag out and giving the reasoning more output tokens. A thinking
Qwen3-8B also tends to answer in English when asked for Vietnamese or Japanese.

## Decision

- Every model task belongs to one of three tiers, fixed in code (`Task::tier`,
  [`llm/profile.rs`](../../crates/gm/src/llm/profile.rs)): **easy** (router, triage, digest line),
  **medium** (summary, digest jobs, translate), **hard** (analyze, reply).
- The parameters per tier live in a YAML file (`GM_LLM_PROFILE`, default [`llm.yaml`](../../llm.yaml)),
  one section per model id, so changing the model means adding a section, not changing code:
  `no_think_tag`, and per tier `thinking`, `thinking_budget` (output tokens added to the task's
  answer cap) and `language_check` (an answer in another language than the owner's is asked again).
- Each call uses the section of the model it is sent to. A missing file or section gives built-in
  defaults (Qwen3: hard thinks with 1,024 tokens and checks language) with a warning; an invalid file
  stops `gm serve`. `gm status` shows the active profile in one line.
- Code calls `LlmClient::run(task, …)` with the answer cap only; it never chooses thinking,
  temperature or retries itself. A failed thinking answer falls back to the answer without thinking;
  a failed language check without thinking asks the same prompt again at temperature 0.

## Consequences

- Answer caps in code are the answer alone; a thinking task's `max_tokens` is the cap plus the
  tier's budget (analyze 1,024 + 1,024, reply 512 + 1,024 by default).
- A wrong-language thinking answer costs a second call. Turning thinking off for the hard tier in
  `llm.yaml` trades that for less careful analysis.
- Moving a task to another tier is a code change (with tests); tuning a tier is a file edit and a
  restart.

## Alternatives considered

- **Parameters per task in the file:** most flexible, but eight entries per model to keep in sync;
  tiers keep the file small and the grouping reviewable.
- **Tier map in the file:** lets the owner move tasks without a build, but a task's parsing and
  fallbacks assume its tier's behavior, so the map stays in code.
- **Environment variables:** one flat namespace can't hold per-model sections.
