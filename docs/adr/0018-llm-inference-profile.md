# 0018. LLM model clients and sampling profiles

- Status: Accepted
- Date: 2026-11-18

## Context

Routing and triage need a short classification. Analyzing a mail or drafting a reply benefits
from more reasoning. Qwen3 switches to a direct answer with `/no_think`. Bonsai's chat template
supplies the opening `<think>` tag as part of the prompt, and the runtime returns generated text
starting inside the reasoning block. mlx-air 0.3 does not expose the template's thinking switch
through its chat endpoint.

## Decision

- Each task belongs to the easy, medium or hard category in
  [`llm/model_client.rs`](../../crates/gm/src/llm/model_client.rs). Qwen3 and Bonsai implement those
  three methods on the same `ModelClient` trait. Each method receives the exact task, so a model
  can share prompt format and retry policy across a category and tune answer and reasoning
  tokens per task.
  Each implementation declares its supported
  model IDs with a `#[distributed_slice(CLIENTS)]` registration in the client's module. Startup
  indexes those registrations by ID and rejects duplicate IDs.
- [`LlmClient`](../../crates/gm/src/llm/mod.rs) selects the client for the served model after
  any residency wake and keeps that model for the entire task. It shares HTTP transport,
  admission, metrics and task parsers across the model clients. Qwen3 registers the default
  client for unrecognized IDs.
- [`llm.yaml`](../../llm.yaml) supplies sampling overrides by model ID
  ([profile specification](../spec/interfaces.md#inference-profile)). Legacy tier and tag keys
  are accepted and ignored so old profile files can still load. `gm status` shows the model
  client and sampling values.
- Qwen3 uses direct answers for easy and medium tasks and falls back to a direct answer if a
  hard task's reasoning pass fails. The [Bonsai client](../../crates/gm/src/llm/model_client/bonsai.rs)
  defines its own answer cap for each task. The direct checkpoint uses those caps as-is.
  The stock checkpoint adds a reasoning allowance and parses the answer after `</think>`.
  The executor sends one combined `max_tokens` limit for reasoning requests.

## Consequences

- Tuning sampling is a file edit and a restart. The Model tab shows effective task budgets and
  can override the existing mail summary input and Qwen3 answer limits. The client defines the
  other task budgets and reasoning allowances. Selecting a model on the Model tab applies at the
  next start.
- Adding another model client leaves the HTTP protocol, task parsers, and task-to-category
  mapping in one place.

## Alternatives considered

- Encode reasoning controls in YAML. This can express prompt tags and token budgets, while
  response formats and retries still require model-specific code.
- Give each client its own HTTP layer. That would duplicate queueing, residency, failure
  handling and metrics.
